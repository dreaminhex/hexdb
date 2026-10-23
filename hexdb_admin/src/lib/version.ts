/** Compare versions such as "2.1.2" and "v2.2.0-rc.1" as the server does (`hexdb_core::updates::compare`). */
export function compareVersions(a: string, b: string): number {
  const split = (v: string) => {
    const [core, pre] = v.trim().replace(/^v/, "").split(/-(.*)/s, 2)
    return { numbers: core.split(".").map((p) => Number.parseInt(p, 10) || 0), pre }
  }
  const x = split(a)
  const y = split(b)
  for (let i = 0; i < Math.max(x.numbers.length, y.numbers.length); i++) {
    const d = (x.numbers[i] ?? 0) - (y.numbers[i] ?? 0)
    if (d !== 0) return Math.sign(d)
  }
  if (x.pre === y.pre) return 0
  if (x.pre === undefined) return 1
  if (y.pre === undefined) return -1
  return x.pre < y.pre ? -1 : 1
}

/** True if `latest` is newer than `running`. */
export function isNewer(latest: string | null | undefined, running: string | null | undefined): boolean {
  return !!latest && !!running && compareVersions(latest, running) > 0
}
