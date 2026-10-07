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
