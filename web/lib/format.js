// Display helpers shared by the pages, exposed as window.Fmt.
window.Fmt = {
  bytes(n) {
    n = Number(n) || 0
    if (n >= 1024 * 1024 * 1024) return `${(n / 1024 / 1024 / 1024).toFixed(2)} GB`
    if (n >= 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`
    if (n >= 1024) return `${(n / 1024).toFixed(1)} KB`
    return `${n} B`
  },

  /** "3 min ago" from Unix seconds. */
  ago(seconds) {
    if (!seconds) return 'never'
    const diff = Math.max(0, Date.now() / 1000 - seconds)
    if (diff < 60) return 'just now'
    if (diff < 3600) return `${Math.floor(diff / 60)} min ago`
    if (diff < 86400) return `${Math.floor(diff / 3600)} h ago`
    if (diff < 86400 * 30) return `${Math.floor(diff / 86400)} d ago`
    return new Date(seconds * 1000).toLocaleDateString()
  },

  date(seconds) {
    return seconds ? new Date(seconds * 1000).toLocaleString() : ''
  },

  duration(seconds) {
    const d = Math.floor(seconds / 86400)
    const h = Math.floor((seconds % 86400) / 3600)
    const m = Math.floor((seconds % 3600) / 60)
    if (d) return `${d}d ${h}h`
    if (h) return `${h}h ${m}m`
    return `${m}m`
  },

  number(n) {
    return (Number(n) || 0).toLocaleString()
  },
}
