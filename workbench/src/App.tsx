import { useEffect, useState } from "react"
import { api, COLORS, type Color, type Difference, type Environment, type Health, type HostKey } from "./api"

const blank: Environment = { id: "", name: "", color: "green", url: "http://127.0.0.1:7700", ssh: null, has_key: false }

function message(e: unknown): string {
  return typeof e === "string" ? e : e instanceof Error ? e.message : String(e)
}

export function App() {
  const [envs, setEnvs] = useState<Environment[]>([])
  const [editing, setEditing] = useState<Environment | null>(null)
  const [tab, setTab] = useState<"environments" | "compare">("environments")
  const [error, setError] = useState<string | null>(null)

  const reload = () => api.list().then(setEnvs, (e) => setError(message(e)))
  useEffect(() => {
    reload()
  }, [])

  return (
    <div className="app">
      <header>
        <h1>HexDB Workbench</h1>
        <nav>
          <button className={tab === "environments" ? "active" : ""} onClick={() => setTab("environments")}>
            Environments
          </button>
          <button className={tab === "compare" ? "active" : ""} onClick={() => setTab("compare")}>
            Compare
          </button>
        </nav>
      </header>
      {error && (
        <p className="error" role="alert" onClick={() => setError(null)}>
          {error}
        </p>
      )}
      {tab === "environments" ? (
        editing ? (
          <EnvironmentForm
            initial={editing}
            onCancel={() => setEditing(null)}
            onSaved={() => {
              setEditing(null)
              reload()
            }}
          />
        ) : (
          <EnvironmentList envs={envs} onEdit={setEditing} onAdd={() => setEditing({ ...blank })} onChanged={reload} onError={setError} />
        )
      ) : (
        <Compare envs={envs} />
      )}
    </div>
  )
}

function EnvironmentList(props: { envs: Environment[]; onEdit: (e: Environment) => void; onAdd: () => void; onChanged: () => void; onError: (m: string) => void }) {
  const [health, setHealth] = useState<Record<string, Health | "checking">>({})
  const [hostKeys, setHostKeys] = useState<{ env: Environment; keys: HostKey[] } | null>(null)

  const check = async (env: Environment) => {
    setHealth((h) => ({ ...h, [env.id]: "checking" }))
    try {
      const result = await api.check(env.id)
      setHealth((h) => ({ ...h, [env.id]: result }))
    } catch (e) {
      setHealth((h) => ({ ...h, [env.id]: { ok: false, version: null, message: message(e) } }))
    }
  }

  if (props.envs.length === 0) {
    return (
      <section className="empty">
        <p>No environments yet. An environment is one lattice: its address, a color, and how to reach it.</p>
        <button className="primary" onClick={props.onAdd}>
          Add an environment
        </button>
      </section>
    )
  }

  return (
    <section>
      <div className="toolbar">
        <button className="primary" onClick={props.onAdd}>
          Add an environment
        </button>
      </div>
      <ul className="envs">
        {props.envs.map((env) => {
          const h = health[env.id]
          return (
            <li key={env.id} className={`env color-${env.color}`}>
              <div className="env-main">
                <strong>{env.name}</strong>
                <span className="muted">
                  {env.url}
                  {env.ssh ? ` via ssh ${env.ssh.user}@${env.ssh.host}` : ""}
                </span>
                {h && <span className={h === "checking" ? "muted" : h.ok ? "good" : "bad"}>{h === "checking" ? "Checking..." : h.message}</span>}
              </div>
              <div className="env-actions">
                <button className="primary" onClick={() => api.open(env.id).catch((e) => props.onError(message(e)))}>
                  Open
                </button>
                <button onClick={() => check(env)}>Check</button>
                {env.ssh && (
                  <button onClick={() => api.scanHost(env.id).then((keys) => setHostKeys({ env, keys }), (e) => props.onError(message(e)))}>Host key</button>
                )}
                <button onClick={() => props.onEdit(env)}>Edit</button>
                <button
                  onClick={() => {
                    if (confirm(`Remove ${env.name}? Its API key is removed from the keychain.`)) api.remove(env.id).then(props.onChanged, (e) => props.onError(message(e)))
                  }}
                >
                  Remove
                </button>
              </div>
            </li>
          )
        })}
      </ul>
      {hostKeys && (
        <div className="dialog" role="dialog" aria-label="Host key">
          <h2>Host key of {hostKeys.env.ssh?.host}</h2>
          <p>Compare these fingerprints with the ones the server's administrator gave you (ssh-keygen -lf on the server). Trust them only if they match.</p>
          <ul className="keys">
            {hostKeys.keys.map((k) => (
              <li key={k.line}>
                <code>{k.kind}</code> <code>{k.fingerprint}</code>
              </li>
            ))}
          </ul>
          <div className="toolbar">
            <button
              className="primary"
              onClick={() =>
                api.trustHost(hostKeys.keys).then(
                  () => setHostKeys(null),
                  (e) => props.onError(message(e)),
                )
              }
            >
              They match: trust this host
            </button>
            <button onClick={() => setHostKeys(null)}>Cancel</button>
          </div>
        </div>
      )}
    </section>
  )
}

function EnvironmentForm(props: { initial: Environment; onCancel: () => void; onSaved: () => void }) {
  const [env, setEnv] = useState<Environment>(props.initial)
  const [key, setKey] = useState("")
  const [error, setError] = useState<string | null>(null)
  const set = (patch: Partial<Environment>) => setEnv((e) => ({ ...e, ...patch }))
  const setSsh = (patch: Partial<NonNullable<Environment["ssh"]>>) => setEnv((e) => ({ ...e, ssh: { ...(e.ssh ?? { host: "", port: 22, user: "", identity_file: "" }), ...patch } }))

  const save = async () => {
    try {
      await api.save(env, key === "" ? undefined : key)
      props.onSaved()
    } catch (e) {
      setError(message(e))
    }
  }

  return (
    <section className="form">
      <h2>{env.id ? `Edit ${props.initial.name}` : "New environment"}</h2>
      <label>
        Name
        <input value={env.name} onChange={(e) => set({ name: e.target.value })} placeholder="Production" maxLength={64} />
      </label>
      <fieldset>
        <legend>Color</legend>
        <div className="colors">
          {COLORS.map((c) => (
            <button key={c} type="button" aria-label={c} aria-pressed={env.color === c} className={`swatch color-${c} ${env.color === c ? "chosen" : ""}`} onClick={() => set({ color: c as Color })} />
          ))}
        </div>
        <p className="muted">The color frames the environment's window, so production never looks like development.</p>
      </fieldset>
      <label>
        Address
        <input value={env.url} onChange={(e) => set({ url: e.target.value })} placeholder="https://acme.hexdb.app" />
      </label>
      <label className="check">
        <input type="checkbox" checked={!!env.ssh} onChange={(e) => set({ ssh: e.target.checked ? { host: "", port: 22, user: "", identity_file: "" } : null })} />
        Reach it through an SSH tunnel (the address is then the far side's, such as http://127.0.0.1:7700)
      </label>
      {env.ssh && (
        <div className="ssh">
          <label>
            SSH host
            <input value={env.ssh.host} onChange={(e) => setSsh({ host: e.target.value })} placeholder="db.example.com" />
          </label>
          <label>
            Port
            <input type="number" value={env.ssh.port} onChange={(e) => setSsh({ port: Number(e.target.value) || 22 })} />
          </label>
          <label>
            User
            <input value={env.ssh.user} onChange={(e) => setSsh({ user: e.target.value })} />
          </label>
          <label>
            Private key file (empty: the SSH agent and your default keys)
            <input value={env.ssh.identity_file} onChange={(e) => setSsh({ identity_file: e.target.value })} />
          </label>
        </div>
      )}
      <label>
        API key for compare and checks {env.has_key ? "(one is saved in the keychain; type a new one to replace it)" : "(optional; kept in the OS keychain)"}
        <input type="password" value={key} onChange={(e) => setKey(e.target.value)} autoComplete="off" />
      </label>
      {env.has_key && (
        <button type="button" onClick={() => api.save(env, "").then(props.onSaved, (e) => setError(message(e)))}>
          Remove the saved API key
        </button>
      )}
      {error && (
        <p className="error" role="alert">
          {error}
        </p>
      )}
      <div className="toolbar">
        <button className="primary" onClick={save}>
          Save
        </button>
        <button onClick={props.onCancel}>Cancel</button>
      </div>
    </section>
  )
}

const WHAT: Record<Difference["what"], string> = {
  only_left: "Tessellation",
  only_right: "Tessellation",
  kind: "Kind",
  index_only_left: "Index",
  index_only_right: "Index",
  index_differs: "Index",
  schema: "Schema",
}

function Compare(props: { envs: Environment[] }) {
  const [left, setLeft] = useState("")
  const [right, setRight] = useState("")
  const [result, setResult] = useState<Difference[] | null>(null)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const name = (id: string) => props.envs.find((e) => e.id === id)?.name ?? ""

  const run = async () => {
    setBusy(true)
    setError(null)
    try {
      setResult(await api.compare(left, right))
    } catch (e) {
      setError(message(e))
      setResult(null)
    } finally {
      setBusy(false)
    }
  }

  return (
    <section>
      <p className="muted">Compares the tessellations, indexes and current schemas of two lattices, such as staging and production. Each needs an API key that can read its tessellations. Nothing is changed.</p>
      <div className="toolbar">
        <select value={left} onChange={(e) => setLeft(e.target.value)} aria-label="Left">
          <option value="">Left...</option>
          {props.envs.map((e) => (
            <option key={e.id} value={e.id}>
              {e.name}
            </option>
          ))}
        </select>
        <select value={right} onChange={(e) => setRight(e.target.value)} aria-label="Right">
          <option value="">Right...</option>
          {props.envs.map((e) => (
            <option key={e.id} value={e.id}>
              {e.name}
            </option>
          ))}
        </select>
        <button className="primary" disabled={!left || !right || left === right || busy} onClick={run}>
          {busy ? "Comparing..." : "Compare"}
        </button>
      </div>
      {error && (
        <p className="error" role="alert">
          {error}
        </p>
      )}
      {result && result.length === 0 && <p className="good">No differences: the same tessellations, indexes and schemas.</p>}
      {result && result.length > 0 && (
        <table>
          <thead>
            <tr>
              <th>Tessellation</th>
              <th>What</th>
              <th>Difference</th>
              <th>{name(left)}</th>
              <th>{name(right)}</th>
            </tr>
          </thead>
          <tbody>
            {result.map((d, i) => (
              <tr key={i}>
                <td>{d.tessellation}</td>
                <td>{WHAT[d.what]}</td>
                <td>{d.detail}</td>
                <td>{d.left != null && <pre>{JSON.stringify(d.left, null, 2)}</pre>}</td>
                <td>{d.right != null && <pre>{JSON.stringify(d.right, null, 2)}</pre>}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </section>
  )
}
