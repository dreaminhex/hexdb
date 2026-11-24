# Plan: HexDB Enterprise on AWS Marketplace

Status: a plan for the owner to file later (2026-11-23). Nothing here is built. The seller account, the tax interview and the bank details are the owner's. Internal document: it names the hosting provider, which customer copy never does.

## What would be listed

HexDB Enterprise as a **SaaS product**. A buyer subscribes through AWS Marketplace, AWS bills them on their AWS invoice (against their committed spend, which is the main reason buyers ask for Marketplace), and they land on hexdb.app to create their lattice, as a direct customer does today. The lattice runs where hosted lattices run now. HexDB itself (the open-source server) needs no listing. A container or AMI listing would be a separate, later product with a 20% fee.

## Before filing: confirm with AWS

1. **Where the product is hosted.** AWS's startup guide describes Marketplace products as "built on AWS", and some programs (ISV Accelerate, co-sell) expect AWS hosting. Hosted lattices run on Fly today. Ask AWS Marketplace seller operations whether a SaaS listing hosted outside AWS is accepted, and on what terms. This decides everything below. If AWS hosting is required, see "If AWS hosting is required".
2. **The current fee schedule.** As of January 2024: 3% on public SaaS offers, and on private offers 3% under $1M total contract value, 2% from $1M, 1.5% from $10M, and 1.5% on renewals (0.5% more through a channel partner).
3. **The notification mechanism.** AWS is replacing SNS subscription notifications with EventBridge for new listings. Build for EventBridge.

## Seller account (the owner's steps)

- An AWS account for selling, separate from any account used for infrastructure, with MFA on the root user.
- Register in the AWS Marketplace Management Portal as Martinforged, LLC (dba Dream In Hex): public profile, the tax interview (W-9), the bank account for disbursements, and the disbursement schedule.
- Optional: the AWS Partner Network and its Software Path, for co-sell later.

## Listing copy (draft)

- **Title:** HexDB Enterprise
- **Short description:** Hosted HexDB lattices: a replicated document database with files, SQL, GraphQL, change streams and AI built in, on dedicated containers in up to three regions.
- **Long description:** HexDB Enterprise runs a HexDB lattice for you: three or more hexes in the regions you choose, with failover that loses no acknowledged write, point-in-time restore to any moment of the last week, encrypted storage with customer-managed keys (AWS KMS or Azure Key Vault), malware scanning of uploads, rolling updates one hex at a time, and an admin UI. Documents, files with metadata extraction and thumbnails, SQL with joins, GraphQL, gRPC, queues, streams and an AI agent that answers questions about your data, all in one database. Billing is per hex by size and region, and changes take effect immediately, prorated.
- **Highlights:** (1) Replicated across regions, failover without lost writes. (2) Point-in-time restore and customer-managed keys. (3) Documents, files, SQL, GraphQL and AI in one database.
- **Categories:** Databases; Data Analytics (secondary).
- **Support:** support@hexdb.app; the support terms from hexdb.app/terms.
- **Legal:** the hexdb.app Terms of Service as the EULA (or AWS's Standard Contract for AWS Marketplace, which many buyers' procurement teams prefer: worth offering both).
- **Assets:** logo (from marketing/), the admin UI screenshot the website uses, a link to the manual.

## Pricing model

Recommendation: a **SaaS subscription** (pay as you go), metered hourly. It matches how hosted lattices are billed now: per hex, by size and region tier, changes prorated at once. Annual commitments go through **private offers** on a SaaS contract when a buyer asks.

Dimensions (one per thing billed, priced per hour from the monthly prices in `hexdb_website/src/lib/pricing.ts` divided by 730):

| Dimension | Unit | Count |
|---|---|---|
| Hex, Small / Standard / Large / X-Large, by region tier (0%, 15%, 35%) | hex-hour | 12 |
| Scanner, 2 GB / 4 GB, by region tier | scanner-hour | 6 |
| Custom domain | domain-hour | 1 |

That's 19 dimensions. Check AWS's current limit per product (it has been 24). Prices on Marketplace should be the same as on hexdb.app plus the fee (about 3%), or the same, absorbing it. Gross margins are 45 to 50% today, so absorbing it is affordable. Free trial: none at first. The playground is the trial.

## The integration it would need (in hexdb_website)

1. **Fulfillment URL** (`POST /api/aws/register`): AWS posts the buyer's browser there with `x-amzn-marketplace-token`. Call `ResolveCustomer` (AWS Marketplace Metering Service) to get `CustomerIdentifier`, `CustomerAWSAccountId` and `ProductCode`. Then sign the buyer in or create their account (the existing email sign-up), link it to the customer identifier, and mark it **billed by AWS**, with no Stripe customer. A token that doesn't resolve is refused.
2. **Subscription events** (EventBridge rule to an endpoint, or SNS while it lasts): `subscribe-success` enables provisioning; `unsubscribe-pending` sends a final metering run; `unsubscribe-success` follows the existing failed-payment path (machines stopped, volumes kept, deleted at day 30, with the same emails); `subscribe-fail` blocks provisioning. Contracts (private offers) use entitlement events and `GetEntitlements` to read what was bought.
3. **Metering:** an hourly job sends `BatchMeterUsage` (up to 25 records a call; records older than 6 hours are refused; idempotent, so retries are safe). It sends one record per customer and dimension for the hex-hours of the past hour, from the same machine inventory Stripe billing uses. Every sent record is kept in the accounts lattice with AWS's answer, so a missed hour is resent within the 6 hours and alerts after that.
4. **Billing switch:** an account is billed by Stripe or by AWS, never both. Plan changes, the scanner and custom domains work the same way, and only the meter changes. The account page shows "Billed through AWS Marketplace" instead of the Stripe portal.
5. **Credentials:** an IAM role for the metering and entitlement calls (`aws-marketplace:ResolveCustomer`, `aws-marketplace:BatchMeterUsage`, `aws-marketplace:GetEntitlements`), kept in Vercel's environment like the Stripe key.
6. **Testing:** AWS gives a limited listing that only allowlisted accounts can see. Subscribe from a test account, check registration, an hour of metering in CloudTrail (`BatchMeterUsage`), plan changes and unsubscribing, before requesting public visibility.

Estimated work: about a week for the integration and tests, then AWS's listing review, which typically takes a few weeks.

## What changes in hosting

- **If a non-AWS-hosted listing is accepted:** nothing in hosting. Buyers may still ask for their lattice in AWS regions near their applications: the existing regions cover the same metros (Ashburn, Oregon, Frankfurt, Tokyo and so on). Private connectivity on Fly (2.7) gives them a path that skips the public internet, but not AWS PrivateLink.
- **If AWS hosting is required, or buyers want PrivateLink:** a second hosting backend would be needed. One ECS or EKS cluster per region, an EC2 instance or Fargate task plus an EBS volume per hex, a Network Load Balancer per lattice, and PrivateLink endpoint services for buyers' VPCs. The control plane's `fly.ts` would get an `aws.ts` sibling behind the same interface (create, start, stop, resize, replace, retire, volumes and certificates). Costs differ: gp3 volumes are cheaper than Fly volumes, but EC2 with an NLB per lattice is dearer at the small size, so the Small size would need repricing. That's a larger project (several weeks) and a decision for the owner, not part of this plan.

## Filing checklist (the owner)

1. Ask AWS the three questions above.
2. Register the seller account (tax, bank, profile).
3. When the integration is built: create the SaaS product (limited), enter the dimensions and prices, the fulfillment URL and the EventBridge setup, upload the copy and assets.
4. Test with an allowlisted account, then request public visibility.

Sources: AWS Marketplace listing fees (docs.aws.amazon.com/marketplace/latest/userguide/listing-fees.html); the metering APIs (docs.aws.amazon.com/marketplace/latest/developerguide/metering-apis.html); the SaaS customer setup and EventBridge notifications (docs.aws.amazon.com/marketplace/latest/userguide/saas-product-customer-setup.html); AWS's startup listing guide.
