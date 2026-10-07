// Seeds a running HexDB with varied, realistic test data through the bulk API:
// articles, products, customers, orders, and short-lived sessions (15-minute TTL).
//
// Usage (Node.js 18+, no dependencies), with the server running:
//
//     node scripts/seed.mjs
//     HEXDB=http://127.0.0.1:7700 node scripts/seed.mjs      # another server
//
// Each tessellation is inserted in one atomic batch and skipped if it already has
// documents, so re-running is safe and only fills in what is missing (e.g. the
// sessions once they expire). Delete a tessellation to reseed it.
const BASE = process.env.HEXDB ?? "http://127.0.0.1:7700"

let state = 20261007
const rand = () => ((state = (state * 1664525 + 1013904223) >>> 0) / 2 ** 32)
const int = (min, max) => Math.floor(rand() * (max - min + 1)) + min
const pick = (list) => list[int(0, list.length - 1)]
const some = (list, min, max) => [...list].sort(() => rand() - 0.5).slice(0, int(min, max))
const money = (min, max) => Math.round((min + rand() * (max - min)) * 100) / 100
const daysAgo = (max) => new Date(Date.now() - rand() * max * 86_400_000).toISOString()

const FIRST = ["Ada", "Grace", "Linus", "Margaret", "Alan", "Barbara", "Dennis", "Frances", "Ken", "Radia", "Tim", "Hedy", "Edsger", "Katherine", "Bjarne", "Sophie"]
const LAST = ["Lovelace", "Hopper", "Torvalds", "Hamilton", "Turing", "Liskov", "Ritchie", "Allen", "Thompson", "Perlman", "Berners-Lee", "Lamarr", "Dijkstra", "Johnson", "Stroustrup", "Wilson"]
const COUNTRIES = ["US", "CA", "GB", "DE", "FR", "JP", "AU", "BR", "IN", "NL"]
const person = () => {
  const name = `${pick(FIRST)} ${pick(LAST)}`
  return { name, email: `${name.toLowerCase().replace(/[^a-z]+/g, ".")}@example.com` }
}

async function count(tess) {
  const res = await fetch(`${BASE}/${tess}/count`)
  if (res.status === 404) return 0
  if (!res.ok) throw new Error(`${tess}: ${res.status} ${await res.text()}`)
  return (await res.json()).count
}

/** Insert documents in one atomic batch, unless the tessellation already has data. Returns the new IDs, or null if skipped. */
async function bulk(tess, docs, { ttl } = {}) {
  const existing = await count(tess)
  if (existing > 0) {
    console.log(`${tess.padEnd(10)} already has ${existing} documents; skipped`)
    return null
  }
  const res = await fetch(`${BASE}/${tess}/_bulk${ttl ? `?ttl=${ttl}` : ""}`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(docs),
  })
  const body = await res.json()
  if (!res.ok) throw new Error(`${tess}: ${res.status} ${JSON.stringify(body)}`)
  console.log(`${tess.padEnd(10)} ${String(body.count).padStart(4)} documents`)
  return body.ids
}

/** IDs of existing customers, in the same order as the generated list (matched by email). */
async function existingCustomerIds(list) {
  const res = await fetch(`${BASE}/customers/_query`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ limit: 1000 }),
  })
  const byEmail = new Map((await res.json()).documents.map((d) => [d.email, d.id]))
  return list.map((c) => byEmail.get(c.email) ?? null)
}

// Articles: text, tags, nested author, booleans, counts, timestamps.
const ADJ = ["Quantum", "Distributed", "Hexagonal", "Lazy", "Immutable", "Concurrent", "Streaming", "Durable", "Elastic", "Adaptive", "Columnar", "Vectorized"]
const NOUN = ["Tessellations", "Write-Ahead Logs", "Vertices", "Compaction", "Indexes", "Lattices", "Snapshots", "Tombstones", "Caches", "Queries", "Shards", "Replicas"]
const TAGS = ["rust", "databases", "storage", "performance", "graphql", "ai", "distributed-systems", "tutorial", "release-notes", "internals"]
const articles = Array.from({ length: 200 }, () => {
  const title = `${pick(ADJ)} ${pick(NOUN)}`
  const views = Math.floor(rand() ** 2 * 5000)
  return {
    title,
    slug: title.toLowerCase().replace(/[^a-z]+/g, "-"),
    category: pick(["engineering", "product", "tutorial", "announcement"]),
    author: person(),
    tags: some(TAGS, 1, 4),
    published: rand() < 0.8,
    featured: rand() < 0.1,
    views,
    likes: Math.floor(views * rand() * 0.15),
    word_count: int(300, 4000),
    published_at: daysAgo(180),
  }
})

// Products: prices, stock (some zero), ratings, nested dimensions.
const PRODUCT = ["Keyboard", "Monitor", "Dock", "Headset", "Webcam", "Mouse", "Desk Lamp", "Chair", "Cable", "SSD", "Router", "Microphone"]
const products = Array.from({ length: 120 }, (_, i) => ({
  sku: `HX-${String(1000 + i)}`,
  name: `${pick(["Pro", "Lite", "Max", "Mini", "Studio", "Travel"])} ${pick(PRODUCT)}`,
  category: pick(["peripherals", "displays", "audio", "furniture", "storage", "networking"]),
  price: money(9, 1299),
  stock: rand() < 0.15 ? 0 : int(1, 400),
  rating: Math.round((2.5 + rand() * 2.5) * 10) / 10,
  tags: some(["wireless", "usb-c", "ergonomic", "4k", "refurbished", "bestseller", "eco"], 0, 3),
  dimensions: { width_cm: int(5, 120), height_cm: int(2, 90), depth_cm: int(2, 70), weight_kg: money(0.1, 25) },
  active: rand() < 0.9,
}))

// Customers.
const customers = Array.from({ length: 80 }, () => ({
  ...person(),
  country: pick(COUNTRIES),
  tier: pick(["bronze", "bronze", "silver", "silver", "gold"]),
  signed_up_at: daysAgo(720),
  lifetime_value: money(0, 9000),
  marketing_opt_in: rand() < 0.6,
}))

console.log(`Seeding ${BASE} ...`)
await bulk("articles", articles)
await bulk("products", products)
const customerIds = (await bulk("customers", customers)) ?? (await existingCustomerIds(customers))

// Orders: arrays of line items referencing product SKUs and customer IDs.
const STATUS = ["pending", "paid", "shipped", "shipped", "delivered", "delivered", "delivered", "cancelled"]
const orders = Array.from({ length: 400 }, (_, i) => {
  const items = Array.from({ length: int(1, 4) }, () => {
    const product = pick(products)
    return { sku: product.sku, name: product.name, qty: int(1, 3), unit_price: product.price }
  })
  const c = int(0, customers.length - 1)
  return {
    order_no: `ORD-${String(20000 + i)}`,
    customer_id: customerIds[c],
    customer: { name: customers[c].name, country: customers[c].country },
    items,
    item_count: items.reduce((n, it) => n + it.qty, 0),
    total: Math.round(items.reduce((sum, it) => sum + it.qty * it.unit_price, 0) * 100) / 100,
    status: pick(STATUS),
    shipping: { method: pick(["standard", "express", "pickup"]), country: customers[c].country },
    placed_at: daysAgo(90),
  }
})
await bulk("orders", orders)

// Sessions expire 15 minutes after seeding, to show TTL on the dashboard.
const sessions = Array.from({ length: 25 }, () => ({
  user: pick(customers).email,
  ip: `10.${int(0, 255)}.${int(0, 255)}.${int(1, 254)}`,
  user_agent: pick(["Firefox 140", "Chrome 141", "Safari 19", "Edge 141"]),
  started_at: new Date().toISOString(),
}))
await bulk("sessions", sessions, { ttl: 900 })

console.log("Done.")
