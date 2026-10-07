export interface QueryExample {
  name: string
  description: string
  query: string
  variables?: Record<string, unknown>
}

/** Starter queries for the console. */
export const QUERY_EXAMPLES: QueryExample[] = [
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
    description: "Documents matching a filter, newest views first",
    query: `# Filters use operators like _gt, _in, _contains, _or.
# In variables you can also write them as $gt, $in, ...
query Documents($tessellation: String!, $filter: JSON) {
  documents(
    tessellation: $tessellation
    filter: $filter
    sort: [{ field: "views", descending: true }]
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
    variables: { tessellation: "articles", filter: { published: true, views: { $gte: 100 } } },
  },
  {
    name: "Count with a filter",
    description: "How many documents match",
    query: `query Count {
  count(tessellation: "articles", filter: { tags: { _contains: "rust" } })
}`,
  },
  {
    name: "Aggregate",
    description: "Group documents and compute counts, sums, and averages",
    query: `# Operators: _count ("*" or a field), _countDistinct, _sum, _avg, _min, _max.
# Rows hold the groupBy fields plus one column per aggregate.
query Revenue {
  aggregate(
    tessellation: "orders"
    filter: { status: { _ne: "cancelled" } }
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
  },
  {
    name: "Full-text search",
    description: "Documents containing every word (fast with a text index)",
    query: `# $text matches whole words, case-insensitively. Create a text index on
# the fields you search (Tessellations > Indexes) to make it fast.
query Search {
  documents(tessellation: "articles", filter: { _text: "rust" }, limit: 10) {
    total
    indexesUsed
    scanned
    documents { id data }
  }
}`,
  },
  {
    name: "Transaction",
    description: "Several writes that all succeed or none do",
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
        { op: "insert", tessellation: "orders", data: { order_no: "ORD-DEMO", status: "pending", total: 0 } },
        { op: "insert", tessellation: "audit", data: { event: "order created", ref: "ORD-DEMO" } },
      ],
    },
  },
  {
    name: "Insert a document",
    description: "Insert and return the stored document",
    query: `mutation Insert($data: JSON!) {
  insertDocument(tessellation: "articles", data: $data) {
    id
    data
  }
}`,
    variables: { data: { title: "Quantum Tessellation", tags: ["hexdb", "rust"], published: false, views: 0 } },
  },
  {
    name: "Update by filter",
    description: "Patch every matching document atomically",
    query: `mutation Publish {
  updateDocuments(
    tessellation: "articles"
    filter: { published: false }
    update: { published: true }
  ) {
    matched
    modified
  }
}`,
  },
  {
    name: "Users and roles",
    description: "Accounts and the roles they hold",
    query: `query Security {
  users {
    login
    emailAddress
    isLocked
    roles {
      name
      permissions
    }
  }
  roles {
    name
    description
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
]
