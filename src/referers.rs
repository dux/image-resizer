// Referer stats per base domain and day, counted in memory and flushed in batches; disabled domains
// are held in memory so a resize never waits on the database for them.

use std::collections::HashSet;
use std::sync::Arc;

use arc_swap::ArcSwap;
use dashmap::DashMap;
use rusqlite::params;
use serde::Serialize;

use crate::store::{self, Store};

/// Base domain of a Referer header: `direct` when absent, `hidden` when it has no host.
pub fn base_domain(referer: &str) -> String {
    if referer.is_empty() {
        return "direct".into();
    }
    match url::Url::parse(referer) {
        Ok(u) => match u.host_str() {
            Some(host) if !host.is_empty() => host.strip_prefix("www.").unwrap_or(host).to_string(),
            _ => "hidden".into(),
        },
        Err(_) => match referer.split('/').nth(2) {
            Some(host) if !host.is_empty() => host.strip_prefix("www.").unwrap_or(host).to_string(),
            _ => "hidden".into(),
        },
    }
}

#[derive(Serialize, schemars::JsonSchema, Clone, Debug, PartialEq, Eq)]
pub struct DomainStat {
    pub domain: String,
    pub total: i64,
    pub disabled: bool,
}

pub struct Referers {
    counts: DashMap<(String, String), i64>,
    disabled: ArcSwap<HashSet<String>>,
}

impl Referers {
    pub fn load(store: &Store) -> store::Result<Self> {
        let disabled = store.read(|c| {
            c.prepare("SELECT domain FROM domains WHERE disabled = 1")?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<HashSet<String>>>()
        })?;
        Ok(Self {
            counts: DashMap::new(),
            disabled: ArcSwap::from_pointee(disabled),
        })
    }

    pub fn count(&self, domain: &str) {
        let day = chrono::Local::now().format("%Y-%m-%d").to_string();
        *self.counts.entry((domain.to_string(), day)).or_insert(0) += 1;
    }

    pub fn is_disabled(&self, domain: &str) -> bool {
        self.disabled.load().contains(domain)
    }

    /// Writes the pending counts in one transaction; failed writes go back into the counter.
    pub fn flush(&self, store: &Store) -> store::Result<()> {
        let pending: Vec<((String, String), i64)> = self
            .counts
            .iter()
            .map(|e| e.key().clone())
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|key| self.counts.remove(&key))
            .collect();
        if pending.is_empty() {
            return Ok(());
        }
        let result = store.write(|c| {
            let tx = c.transaction()?;
            {
                let mut stmt = tx.prepare_cached(
                    "INSERT INTO referer_daily (domain, day, count) VALUES (?1, ?2, ?3)
                     ON CONFLICT (domain, day) DO UPDATE SET count = count + excluded.count",
                )?;
                for ((domain, day), n) in &pending {
                    stmt.execute(params![domain, day, n])?;
                }
            }
            tx.commit()
        });
        if result.is_err() {
            for (key, n) in pending {
                *self.counts.entry(key).or_insert(0) += n;
            }
        }
        result
    }

    /// Flips a domain and returns whether it is now disabled.
    pub fn toggle(&self, store: &Store, domain: &str) -> store::Result<bool> {
        let disabled = !self.is_disabled(domain);
        store.write(|c| {
            c.execute(
                "INSERT INTO domains (domain, disabled) VALUES (?1, ?2)
                 ON CONFLICT (domain) DO UPDATE SET disabled = excluded.disabled",
                params![domain, disabled],
            )
        })?;
        let mut set = HashSet::clone(&self.disabled.load());
        if disabled {
            set.insert(domain.to_string());
        } else {
            set.remove(domain);
        }
        self.disabled.store(Arc::new(set));
        Ok(disabled)
    }

    /// Totals per domain, busiest first, including disabled domains without traffic.
    pub fn stats(&self, store: &Store) -> store::Result<Vec<DomainStat>> {
        self.flush(store)?;
        store.read(|c| {
            c.prepare(
                "SELECT d.domain, COALESCE(SUM(r.count), 0) AS total, COALESCE(MAX(x.disabled), 0)
                 FROM (SELECT domain FROM referer_daily UNION SELECT domain FROM domains) d
                 LEFT JOIN referer_daily r ON r.domain = d.domain
                 LEFT JOIN domains x ON x.domain = d.domain
                 GROUP BY d.domain ORDER BY total DESC, d.domain",
            )?
            .query_map([], |r| {
                Ok(DomainStat {
                    domain: r.get(0)?,
                    total: r.get(1)?,
                    disabled: r.get(2)?,
                })
            })?
            .collect()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_domains() {
        assert_eq!(base_domain(""), "direct");
        assert_eq!(base_domain("https://www.example.com/page"), "example.com");
        assert_eq!(base_domain("https://example.com:8080/page"), "example.com");
        assert_eq!(base_domain("http://sub.example.com"), "sub.example.com");
        assert_eq!(base_domain("garbage"), "hidden");
        assert_eq!(base_domain("about:blank"), "hidden");
    }

    #[test]
    fn counts_toggles_and_stats() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let referers = Referers::load(&store).unwrap();
        referers.count("a.com");
        referers.count("a.com");
        referers.count("b.com");
        assert!(referers.toggle(&store, "b.com").unwrap());
        assert!(referers.is_disabled("b.com"));
        let stats = referers.stats(&store).unwrap();
        assert_eq!(
            stats,
            vec![
                DomainStat {
                    domain: "a.com".into(),
                    total: 2,
                    disabled: false
                },
                DomainStat {
                    domain: "b.com".into(),
                    total: 1,
                    disabled: true
                },
            ]
        );
        // survives a restart
        assert!(Referers::load(&store).unwrap().is_disabled("b.com"));
        assert!(!referers.toggle(&store, "b.com").unwrap());
        assert!(!Referers::load(&store).unwrap().is_disabled("b.com"));
    }
}
