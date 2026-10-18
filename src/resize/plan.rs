// Resize geometry, the Go rules as a pure function: no upscaling for fits, cover + crop with 30% top focus.

use super::params::ResizeParams;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// Keep the image as it is.
    Keep,
    /// Scale to exactly `width` x `height`.
    Scale { width: i32, height: i32 },
    /// Scale to `width` x `height`, then cut the `crop` box `(left, top, width, height)`.
    Cover {
        width: i32,
        height: i32,
        crop: (i32, i32, i32, i32),
    },
}

fn scaled(size: i32, scale: f64) -> i32 {
    ((f64::from(size) * scale).round() as i32).max(1)
}

pub fn plan(orig_w: i32, orig_h: i32, params: &ResizeParams, max_size: i32) -> Plan {
    let tw = params.width.min(max_size);
    let th = params.height.min(max_size);
    if (tw == 0 && th == 0) || orig_w <= 0 || orig_h <= 0 {
        return Plan::Keep;
    }

    if params.crop {
        if tw >= orig_w && th >= orig_h {
            return Plan::Keep;
        }
        let scale = (f64::from(tw) / f64::from(orig_w)).max(f64::from(th) / f64::from(orig_h));
        let (cw, ch) = (scaled(orig_w, scale), scaled(orig_h, scale));
        let left = ((cw - tw) / 2).max(0);
        let top = ((f64::from(ch - th) * 0.3) as i32).max(0);
        let width = tw.min(cw - left);
        let height = th.min(ch - top);
        return Plan::Cover {
            width: cw,
            height: ch,
            crop: (left, top, width, height),
        };
    }

    let scale = if tw > 0 && th > 0 {
        if tw >= orig_w && th >= orig_h {
            return Plan::Keep;
        }
        (f64::from(tw) / f64::from(orig_w)).min(f64::from(th) / f64::from(orig_h))
    } else if tw > 0 {
        if tw >= orig_w {
            return Plan::Keep;
        }
        f64::from(tw) / f64::from(orig_w)
    } else {
        if th >= orig_h {
            return Plan::Keep;
        }
        f64::from(th) / f64::from(orig_h)
    };
    Plan::Scale {
        width: scaled(orig_w, scale),
        height: scaled(orig_h, scale),
    }
}

/// Fit within `max` on both sides, never enlarging; `None` when already within.
pub fn fit_max(orig_w: i32, orig_h: i32, max: i32) -> Option<(i32, i32)> {
    if orig_w <= max && orig_h <= max {
        return None;
    }
    let scale = (f64::from(max) / f64::from(orig_w)).min(f64::from(max) / f64::from(orig_h));
    Some((scaled(orig_w, scale), scaled(orig_h, scale)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(width: i32, height: i32, crop: bool) -> ResizeParams {
        ResizeParams {
            width,
            height,
            crop,
            ..Default::default()
        }
    }

    #[test]
    fn fits() {
        assert_eq!(
            plan(200, 150, &p(100, 0, false), 1600),
            Plan::Scale {
                width: 100,
                height: 75
            }
        );
        assert_eq!(
            plan(200, 150, &p(0, 75, false), 1600),
            Plan::Scale {
                width: 100,
                height: 75
            }
        );
        assert_eq!(
            plan(200, 150, &p(100, 100, false), 1600),
            Plan::Scale {
                width: 100,
                height: 75
            }
        );
        assert_eq!(
            plan(200, 150, &p(300, 100, false), 1600),
            Plan::Scale {
                width: 133,
                height: 100
            }
        );
    }

    #[test]
    fn never_upscales_a_fit() {
        assert_eq!(plan(200, 150, &p(300, 0, false), 1600), Plan::Keep);
        assert_eq!(plan(200, 150, &p(200, 0, false), 1600), Plan::Keep);
        assert_eq!(plan(200, 150, &p(0, 400, false), 1600), Plan::Keep);
        assert_eq!(plan(200, 150, &p(300, 300, false), 1600), Plan::Keep);
        assert_eq!(plan(200, 150, &p(0, 0, false), 1600), Plan::Keep);
    }

    #[test]
    fn covers_and_crops_with_top_focus() {
        assert_eq!(
            plan(200, 150, &p(100, 100, true), 1600),
            Plan::Cover {
                width: 133,
                height: 100,
                crop: (16, 0, 100, 100)
            }
        );
        assert_eq!(
            plan(150, 200, &p(100, 100, true), 1600),
            Plan::Cover {
                width: 100,
                height: 133,
                crop: (0, 9, 100, 100)
            }
        );
        // only one side smaller: Go scales up to cover
        assert_eq!(
            plan(200, 150, &p(300, 100, true), 1600),
            Plan::Cover {
                width: 300,
                height: 225,
                crop: (0, 37, 300, 100)
            }
        );
        assert_eq!(plan(200, 150, &p(300, 300, true), 1600), Plan::Keep);
    }

    #[test]
    fn clamps_to_max_size() {
        assert_eq!(
            plan(4000, 2000, &p(5000, 0, false), 1600),
            Plan::Scale {
                width: 1600,
                height: 800
            }
        );
        assert_eq!(fit_max(4000, 2000, 1600), Some((1600, 800)));
        assert_eq!(fit_max(1600, 900, 1600), None);
    }
}
