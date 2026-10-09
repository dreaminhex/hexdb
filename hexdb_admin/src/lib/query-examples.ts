export interface QueryExample {
  name: string
  description: string
  query: string
  variables?: Record<string, unknown>
}

export interface SqlExample {
  name: string
  description: string
  sql: string
  /** Values for the statement's `?` or `$1` placeholders. */
  params?: unknown[]
}

/** Prefer these names when the server has them; otherwise the first user tessellation. */
const PREFERRED = ["orders", "products", "customers"]

/** The tessellation the read examples target, and whether it is the sample `orders` shape. */
function pickMain(tessellations: string[]) {
  const user = tessellations.filter((t) => !t.startsWith("_"))
  const main = PREFERRED.find((t) => user.includes(t)) ?? user[0] ?? "orders"
  return { user, main, orders: main === "orders", writable: user.includes("sandbox") ? "sandbox" : main }
}

/**
 * Starter queries for the console, written against the tessellations this hex
 * actually has. Reads target `orders` when it exists (the sample data in
 * scripts/seed.mjs and the public playground both have it, with `status`,
 * `total`, `placed_at` and `customer.country`); writes go to `sandbox` when it
 * exists, since on a shared demo that is the one visitors may change.
 */
export function queryExamples(tessellations: string[]): QueryExample[] {
  const { user, main, orders, writable } = pickMain(tessellations)
  const payments = user.includes("payments")

  const examples: QueryExample[] = [
    {
      name: "Tessellations",
      description: "Every tessellation with its document count",
      query: `query Tessellations {
  tessellations {
    name
    kind
    created
    documentCount
  }
}`,
    },
    {
      name: "Filter, sort and page",
      description: orders ? "Delivered orders over 100, newest first" : "Documents matching a filter, 25 at a time",
      query: `# Filters use operators like _gt, _in, _contains, _or.
# In variables you can also write them as $gt, $in, ...
query Documents($tessellation: String!, $filter: JSON) {
  documents(
    tessellation: $tessellation
    filter: $filter${orders ? '\n    sort: [{ field: "placed_at", descending: true }]' : ""}
    limit: 25
  ) {
    total
    documents {
      id
      expiresAt
      data
    }
  }
}`,
      variables: { tessellation: main, filter: orders ? { status: "delivered", total: { $gte: 100 } } : {} },
    },
    {
      name: "Count with a filter",
      description: "How many documents match",
      query: orders
        ? `query Count {
  count(tessellation: "orders", filter: { status: { _in: ["refunded", "cancelled"] } })
}`
        : `query Count {
  count(tessellation: "${main}", filter: {})
}`,
    },
  ]

  if (orders) {
    examples.push({
      name: "Aggregate",
      description: "Revenue by country: counts, sums, averages",
      query: `# Operators: _count ("*" or a field), _countDistinct, _sum, _avg, _min, _max.
# Rows hold the groupBy fields plus one column per aggregate.
query Revenue {
  aggregate(
    tessellation: "orders"
    filter: { status: { _nin: ["cancelled", "refunded"] } }
    groupBy: ["customer.country"]
    aggregates: {
      orders: { _count: "*" }
      revenue: { _sum: "total" }
      averageOrder: { _avg: "total" }
      largest: { _max: "total" }
    }
    sort: [{ field: "revenue", descending: true }]
    limit: 10
  ) {
    totalGroups
    matched
    rows
  }
}`,
    })
  }

  if (payments) {
    examples.push({
      name: "Payments by method",
      description: "Captured charges: how many, how much, and the fees",
      query: `query Payments {
  aggregate(
    tessellation: "payments"
    filter: { type: "charge", status: "captured" }
    groupBy: ["method"]
    aggregates: {
      charges: { _count: "*" }
      amount: { _sum: "amount" }
      fees: { _sum: "fee" }
    }
    sort: [{ field: "amount", descending: true }]
  ) {
    rows
  }
}

query FailedCharges {
  aggregate(
    tessellation: "payments"
    filter: { status: "failed" }
    groupBy: ["decline_code"]
    aggregates: { attempts: { _count: "*" } }
    sort: [{ field: "attempts", descending: true }]
  ) {
    rows
  }
}`,
    })
  }

  examples.push(
    {
      name: "Full-text search",
      description: "Documents containing every word (fast with a text index)",
      query: `# $text matches whole words, case-insensitively, in every string field.
# Create a text index on the fields you search (Tessellations > Indexes) to make it fast.
query Search {
  documents(tessellation: "${main}", filter: { _text: "${orders ? "keyboard" : "hexdb"}" }, limit: 10) {
    total
    indexesUsed
    scanned
    documents { id data }
  }
}`,
    },
    {
      name: "Transaction",
      description: `Several writes to ${writable} that all succeed or none do`,
      query: `# Ops: get, check, insert, replace, patch, delete. Preconditions:
# if_version (from a get, or 0 = must not exist) and if_match (a filter).
# If any operation fails, nothing is written.
mutation Restock($ops: JSON!) {
  transaction(operations: $ops) {
    writes
    results { op tessellation id version document }
  }
}`,
      variables: {
        ops: [
          { op: "insert", tessellation: writable, data: { order_no: "TXN-DEMO-1", status: "pending", total: 42.5, customer: { name: "Ada Lovelace", country: "GB" } } },
          { op: "insert", tessellation: writable, data: { order_no: "TXN-DEMO-2", status: "pending", total: 18, customer: { name: "Grace Hopper", country: "US" } } },
        ],
      },
    },
    {
      name: "Insert a document",
      description: `Insert into ${writable} and return the stored document`,
      query: `mutation Insert($data: JSON!) {
  insertDocument(tessellation: "${writable}", data: $data) {
    id
    data
  }
}`,
      variables: { data: { order_no: "SBX-DEMO", status: "pending", total: 99.95, channel: "web", customer: { name: "Linus Torvalds", country: "US" }, placed_at: new Date().toISOString() } },
    },
    {
      name: "Update by filter",
      description: `Patch every matching document in ${writable} atomically`,
      query: `mutation MarkPaid {
  updateDocuments(
    tessellation: "${writable}"
    filter: { status: "pending" }
    update: { status: "paid" }
  ) {
    matched
    modified
  }
}`,
    },
    {
      name: "Users and roles",
      description: "Accounts and the roles they hold (administrators)",
      query: `query Security {
  users {
    login
    emailAddress
    isLocked
    roles {
      name
      tessellations
    }
  }
  roles {
    name
    description
    permissions
  }
}`,
    },
    {
      name: "Server status",
      description: "Metrics for this hex",
      query: `query Status {
  status
}`,
    },
  )
  return examples
}

/**
 * Starter SQL statements, written against the same tessellations. SQL is
 * read-only: one SELECT over one tessellation, translated onto the query or
 * aggregation REST runs, so the same indexes and permissions apply.
 */
export function sqlExamples(tessellations: string[]): SqlExample[] {
  const { user, main, orders } = pickMain(tessellations)
  const examples: SqlExample[] = []

  if (orders) {
    examples.push(
      {
        name: "Filter, sort and page",
        description: "Delivered orders, newest first, with ? parameters",
        sql: `-- Dotted paths reach into nested objects; AS renames the column.
-- ? placeholders take their values from the Parameters tab, in order.
SELECT order_no, status, total, customer.country AS country, placed_at
FROM orders
WHERE status = ? AND total >= ?
ORDER BY placed_at DESC
LIMIT 25`,
        params: ["delivered", 100],
      },
      {
        name: "Count",
        description: "An exact count, without reading the documents",
        sql: `SELECT COUNT(*) AS orders FROM orders WHERE status IN ('refunded', 'cancelled')`,
      },
      {
        name: "Group and aggregate",
        description: "Revenue by country with COUNT, SUM and AVG",
        sql: `-- GROUP BY accepts fields, aliases or positions; HAVING filters the groups.
SELECT customer.country AS country,
       COUNT(*)   AS orders,
       SUM(total) AS revenue,
       AVG(total) AS average_order,
       MAX(total) AS largest
FROM orders
WHERE status NOT IN ('cancelled', 'refunded')
GROUP BY country
HAVING COUNT(*) > 10
ORDER BY revenue DESC
LIMIT 10`,
      },
      {
        name: "Numbered parameters",
        description: "$1 and $2 placeholders, LIKE and BETWEEN",
        sql: `-- LIKE takes % at the start or end only (prefix, suffix or substring).
SELECT order_no, customer.name AS customer, customer.email AS email, total
FROM orders
WHERE customer.email LIKE $1
  AND total BETWEEN $2 AND $3
ORDER BY total DESC
LIMIT 20`,
        params: ["%@example.com", 250, 2000],
      },
    )
  } else {
    examples.push(
      {
        name: "Select with paging",
        description: `The first 25 documents of ${main}`,
        sql: `-- SELECT * returns id and then every field present in the page's documents.
SELECT * FROM ${main} LIMIT 25`,
      },
      {
        name: "Count",
        description: "An exact count, without reading the documents",
        sql: `SELECT COUNT(*) AS documents FROM ${main}`,
      },
    )
  }

  if (user.includes("payments")) {
    examples.push({
      name: "Payments by method",
      description: "Captured charges: how many, how much, and the fees",
      sql: `SELECT method, COUNT(*) AS charges, SUM(amount) AS amount, SUM(fee) AS fees
FROM payments
WHERE type = 'charge' AND status = 'captured'
GROUP BY method
ORDER BY amount DESC`,
    })
  }

  if (user.includes("products")) {
    examples.push({
      name: "Products in stock",
      description: "A category's products, dearest first",
      sql: `SELECT sku, name, brand, price, stock, rating
FROM products
WHERE category = ? AND stock > 0 AND active IS TRUE
ORDER BY price DESC
LIMIT 20`,
      params: ["audio"],
    })
  }

  if (user.includes("sandbox")) {
    examples.push({
      name: "The sandbox",
      description: "Pending sandbox orders, largest first",
      sql: `SELECT order_no, status, total, customer.name AS customer
FROM sandbox
WHERE status = 'pending'
ORDER BY total DESC
LIMIT 25`,
    })
  }

  examples.push({
    name: "Distinct values",
    description: `Every distinct value of a field in ${orders ? "orders" : main}`,
    sql: orders ? `SELECT DISTINCT status FROM orders ORDER BY status` : `SELECT DISTINCT id FROM ${main} LIMIT 25`,
  })

  return examples
}

/** The examples before the tessellation list has loaded. */
export const QUERY_EXAMPLES: QueryExample[] = queryExamples([])
export const SQL_EXAMPLES: SqlExample[] = sqlExamples([])
