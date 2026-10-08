export interface QueryExample {
  name: string
  description: string
  query: string
  variables?: Record<string, unknown>
}

/** Prefer these names when the server has them; otherwise the first user tessellation. */
const PREFERRED = ["orders", "articles", "products", "customers"]

/**
 * Starter queries for the console, written against the tessellations this hex
 * actually has. Reads target `orders` when it exists (the sample data in
 * scripts/seed.mjs and the public playground both have it, with `status`,
 * `total`, `placed_at` and `customer.country`); writes go to `sandbox` when it
 * exists, since on a shared demo that is the one visitors may change.
 */
export function queryExamples(tessellations: string[]): QueryExample[] {
  const user = tessellations.filter((t) => !t.startsWith("_"))
  const main = PREFERRED.find((t) => user.includes(t)) ?? user[0] ?? "orders"
  const orders = main === "orders"
  const writable = user.includes("sandbox") ? "sandbox" : main
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

/** The examples before the tessellation list has loaded. */
export const QUERY_EXAMPLES: QueryExample[] = queryExamples([])
