// Where a hex's region is. HexDB reports `identity.region` as given (Fly.io sets it from
// FLY_REGION); these are the names for the codes people use most. Unknown codes are shown as is.

export interface Place {
  city: string
  country: string
}

const places: Record<string, Place> = {
  // Fly.io
  ams: { city: "Amsterdam", country: "Netherlands" },
  arn: { city: "Stockholm", country: "Sweden" },
  atl: { city: "Atlanta", country: "United States" },
  bog: { city: "Bogotá", country: "Colombia" },
  bom: { city: "Mumbai", country: "India" },
  bos: { city: "Boston", country: "United States" },
  cdg: { city: "Paris", country: "France" },
  den: { city: "Denver", country: "United States" },
  dfw: { city: "Dallas", country: "United States" },
  ewr: { city: "Secaucus", country: "United States" },
  eze: { city: "Ezeiza", country: "Argentina" },
  fra: { city: "Frankfurt", country: "Germany" },
  gdl: { city: "Guadalajara", country: "Mexico" },
  gig: { city: "Rio de Janeiro", country: "Brazil" },
  gru: { city: "São Paulo", country: "Brazil" },
  hkg: { city: "Hong Kong", country: "Hong Kong" },
  iad: { city: "Ashburn", country: "United States" },
  jnb: { city: "Johannesburg", country: "South Africa" },
  lax: { city: "Los Angeles", country: "United States" },
  lhr: { city: "London", country: "United Kingdom" },
  mad: { city: "Madrid", country: "Spain" },
  mia: { city: "Miami", country: "United States" },
  nrt: { city: "Tokyo", country: "Japan" },
  ord: { city: "Chicago", country: "United States" },
  otp: { city: "Bucharest", country: "Romania" },
  phx: { city: "Phoenix", country: "United States" },
  qro: { city: "Querétaro", country: "Mexico" },
  scl: { city: "Santiago", country: "Chile" },
  sea: { city: "Seattle", country: "United States" },
  sin: { city: "Singapore", country: "Singapore" },
  sjc: { city: "San Jose", country: "United States" },
  syd: { city: "Sydney", country: "Australia" },
  waw: { city: "Warsaw", country: "Poland" },
  yul: { city: "Montreal", country: "Canada" },
  yyz: { city: "Toronto", country: "Canada" },
  // AWS
  "us-east-1": { city: "N. Virginia", country: "United States" },
  "us-east-2": { city: "Ohio", country: "United States" },
  "us-west-1": { city: "N. California", country: "United States" },
  "us-west-2": { city: "Oregon", country: "United States" },
  "ca-central-1": { city: "Montreal", country: "Canada" },
  "eu-west-1": { city: "Ireland", country: "Ireland" },
  "eu-west-2": { city: "London", country: "United Kingdom" },
  "eu-west-3": { city: "Paris", country: "France" },
  "eu-central-1": { city: "Frankfurt", country: "Germany" },
  "eu-north-1": { city: "Stockholm", country: "Sweden" },
  "ap-southeast-1": { city: "Singapore", country: "Singapore" },
  "ap-southeast-2": { city: "Sydney", country: "Australia" },
  "ap-northeast-1": { city: "Tokyo", country: "Japan" },
  "ap-south-1": { city: "Mumbai", country: "India" },
  "sa-east-1": { city: "São Paulo", country: "Brazil" },
  // Azure
  eastus: { city: "Virginia", country: "United States" },
  eastus2: { city: "Virginia", country: "United States" },
  westus2: { city: "Washington", country: "United States" },
  centralus: { city: "Iowa", country: "United States" },
  westeurope: { city: "Netherlands", country: "Netherlands" },
  northeurope: { city: "Ireland", country: "Ireland" },
  uksouth: { city: "London", country: "United Kingdom" },
  // Google Cloud
  "us-central1": { city: "Iowa", country: "United States" },
  "us-east4": { city: "N. Virginia", country: "United States" },
  "us-west1": { city: "Oregon", country: "United States" },
  "europe-west1": { city: "Belgium", country: "Belgium" },
  "europe-west2": { city: "London", country: "United Kingdom" },
  "europe-west3": { city: "Frankfurt", country: "Germany" },
  "asia-northeast1": { city: "Tokyo", country: "Japan" },
}

/** The place for a region code, if it's a known one. */
export function place(region: string | null | undefined): Place | undefined {
  return region ? places[region.trim().toLowerCase()] : undefined
}

/** "Chicago (ord)", "eu-west-9", or "No region". */
export function describeRegion(region: string | null | undefined): string {
  if (!region) return "No region"
  const p = place(region)
  return p ? `${p.city} (${region})` : region
}
