# ⚡ ferrocrawl

**A high-performance, distributed web crawler and data ingestion framework — built in Rust.**

Async-first. Schema-driven. Solana-aware. Ships benchmarks, not promises.

[Getting Started](#getting-started) · [Architecture](#architecture) · [Configuration](#configuration) · [Solana Module](#solana-module) · [Benchmarks](#benchmarks) · [Contributing](#contributing)

<div>
<img src="https://img.shields.io/badge/rust-1.78+-orange?style=flat-square&logo=rust" alt="Rust 1.78+"/>
<img src="https://img.shields.io/badge/license-MIT-blue?style=flat-square" alt="MIT License"/>
<img src="https://img.shields.io/badge/status-active-brightgreen?style=flat-square" alt="Status"/>
<img src="https://img.shields.io/badge/tokio-async-purple?style=flat-square&logo=tokio" alt="Tokio"/>
<img src="https://img.shields.io/badge/solana-enabled-9945FF?style=flat-square&logo=solana" alt="Solana"/>
</div>

---

## What is ferrocrawl?

ferrocrawl is a production-grade data ingestion engine written in Rust. It is designed for teams that need clean, structured data at scale — whether for AI training pipelines, off-chain Web3 indexing, or general-purpose web data extraction.

It is not a scraping script. It is a framework: you define schemas, point it at URLs, and data comes out the other side as Parquet, NDJSON, or rows in ClickHouse.

```
Seed URLs → Scheduler → Dedup (Bloom) → Rate limiter → Async fetcher
         → Parser → Schema extractor → Sink (Parquet / ClickHouse / NDJSON)
```

Distributed workers are coordinated via Redis Streams. A single node handles 10k+ pages per second. The Solana module indexes on-chain state (DEX pools, token metadata, program accounts) and correlates it with off-chain web data in a unified pipeline.

---

## Features

| Category | Capability |
|---|---|
| **Fetching** | Async HTTP/1.1 + HTTP/2, connection pooling, brotli/gzip, custom TLS |
| **Deduplication** | Probabilistic Bloom filter (0.1% FPR at 100M URLs) + exact seen-set |
| **Rate limiting** | Per-domain token bucket via `governor`, configurable burst |
| **Proxy rotation** | Round-robin + failure-aware proxy pool, sticky sessions optional |
| **Extraction** | TOML-defined schemas: CSS selectors → typed fields, regex transforms |
| **Distribution** | Redis Streams consumer groups, automatic dead-letter requeue |
| **Solana** | RPC scraper, DEX pool indexer (Raydium/Orca), Metaplex token metadata |
| **Export** | Parquet (batched row groups), NDJSON streaming, ClickHouse HTTP insert |
| **Observability** | Prometheus metrics, `tracing` spans, Grafana dashboard included |
| **Control plane** | Axum REST API — submit jobs, inspect frontier, register workers |
| **CLI** | `crawl`, `worker`, `export`, `bench` subcommands via `clap` |

---

## Getting Started

### Prerequisites

- Rust 1.78+ (`rustup update stable`)
- Redis 7+ (for distributed mode)
- ClickHouse (optional, for ClickHouse sink)

### Installation

```bash
# Clone the repo
git clone https://github.com/yourusername/ferrocrawl
cd ferrocrawl

# Build all crates in release mode
cargo build --release

# Verify the CLI is working
./target/release/ferrocrawl --version
```

### Your first crawl (30 seconds)

Create a schema file:

```toml
# schemas/hn.toml
[[extractor]]
name = "hn_post"
url_pattern = "news\\.ycombinator\\.com/item"

[[extractor.fields]]
name  = "title"
selector = "td.title > span.titleline > a"
attr  = "text"

[[extractor.fields]]
name  = "score"
selector = "span.score"
attr  = "text"
transform = "parse_int"

[[extractor.fields]]
name  = "author"
selector = "a.hnuser"
attr  = "text"
```

Run the crawl:

```bash
ferrocrawl crawl \
  --seed "https://news.ycombinator.com" \
  --schema schemas/hn.toml \
  --output output/hn.parquet \
  --concurrency 200 \
  --depth 3
```

That's it. Parquet file lands in `output/`.

---

### Crate layout

```
ferrocrawl/
├── crates/
│   ├── fc-core/        # Shared types, traits, config structs
│   ├── fc-engine/      # HTTP fetcher, HTML parser, extractor
│   ├── fc-scheduler/   # Frontier, Bloom filter, rate limiter
│   ├── fc-export/      # Parquet, NDJSON, ClickHouse sinks
│   ├── fc-api/         # Axum REST control plane
│   ├── fc-solana/      # Solana RPC + DEX + metadata module
│   └── fc-cli/         # Binary entrypoint (clap)
├── schemas/            # Example extraction schemas (TOML)
├── config/             # Default configuration files
├── infra/              # docker-compose, Prometheus config, Grafana dashboard
├── benches/            # Criterion benchmark suite
└── docs/               # mdBook source
```

---

## Configuration

ferrocrawl is configured via a TOML file with environment variable overrides.

```toml
# crawler.toml

[crawler]
concurrency     = 500          # max simultaneous in-flight requests
depth           = 5            # max crawl depth from seed
user_agent      = "ferrocrawl/0.1 (+https://github.com/you/ferrocrawl)"
request_timeout = 30           # seconds

[scheduler]
max_queue_size  = 10_000_000
priority_mode   = "breadth"    # "breadth" | "depth" | "score"

[dedup]
bloom_capacity  = 100_000_000  # expected unique URLs
bloom_fpr       = 0.001        # false positive rate

[rate_limiter]
default_rps     = 10           # requests per second per domain
burst           = 20

[proxy]
enabled         = false
rotation        = "round_robin"
proxies         = [
  "http://proxy1:8080",
  "http://proxy2:8080",
]

[export]
sink            = "parquet"    # "parquet" | "ndjson" | "clickhouse"
output_dir      = "./output"
parquet_batch   = 50_000       # rows per row group

[clickhouse]
url             = "http://localhost:8123"
database        = "crawler"
table           = "records"

[redis]
url             = "redis://localhost:6379"
stream          = "fc:jobs"
consumer_group  = "workers"

[api]
host            = "0.0.0.0"
port            = 8080
auth_token      = ""           # set to enable bearer token auth
```

All keys can be overridden via environment variables prefixed `FC_`:

```bash
FC_CRAWLER_CONCURRENCY=1000 FC_EXPORT_SINK=clickhouse ferrocrawl crawl ...
```

---

## Distributed Mode

Start the coordinator (holds the frontier, exposes the API):

```bash
ferrocrawl crawl \
  --seed "https://example.com" \
  --schema schemas/example.toml \
  --distributed \
  --redis redis://localhost:6379
```

Start as many worker nodes as you need (on the same machine or different ones):

```bash
# On each worker machine
ferrocrawl worker \
  --redis redis://localhost:6379 \
  --concurrency 500
```

Workers register themselves with the coordinator and pull jobs from the Redis Stream. If a worker dies, its pending jobs are automatically requeued after a configurable visibility timeout.

---

## Solana Module

The `fc-solana` module treats the Solana network as a first-class data source alongside the web crawler. It runs in the same pipeline and outputs to the same sinks.

```bash
ferrocrawl crawl \
  --solana-rpc "https://api.mainnet-beta.solana.com" \
  --solana-mode dex_pools \
  --output output/solana_pools.parquet
```

### Modes

**`rpc`** — polls configurable RPC methods on a schedule. Supports `getAccountInfo`, `getProgramAccounts`, `getTokenAccountsByOwner`. Decoded with `borsh`.

**`dex_pools`** — subscribes to Raydium and Orca AMM program accounts via WebSocket (`accountSubscribe`). Decodes pool state to extract reserves, prices, fee tiers, and liquidity in real time.

**`token_metadata`** — fetches and decodes Metaplex `DataV2` metadata for any list of mint addresses. Correlates with off-chain data (CoinGecko, Solscan) using the general web crawler.

**`explorer`** — runs the general web crawler against Solscan / SolanaFM with pre-built extractor schemas bundled in the crate.

---

## Benchmarks

Benchmarks run against a local mock HTTP server (`wiremock`) to remove network variance. Run them yourself:

```bash
cargo bench --bench throughput
```

| Scenario | Hardware | Result |
|---|---|---|
| Single-node, 500 concurrent | 8-core / 32GB | **11,400 pages/sec** |
| Single-node, 200 concurrent | 4-core / 16GB | **5,200 pages/sec** |
| Bloom filter (100M URLs) | — | **< 4MB RSS** |
| Parquet write | — | **1.3M rows/min** |
| Solana DEX pool indexing | — | **~12k accounts/min** |

Memory profile: ferrocrawl holds the Bloom filter in memory and streams everything else. The filter for 100M URLs at 0.1% FPR requires ~180MB using 7 hash functions at 10 bits/element.

---

## Observability

Prometheus metrics are exposed at `GET /metrics` on the control plane API.

Key metrics:

| Metric | Type | Description |
|---|---|---|
| `fc_pages_per_second` | Gauge | Current crawl throughput |
| `fc_queue_depth` | Gauge | URLs pending in frontier |
| `fc_bloom_fill_ratio` | Gauge | Bloom filter saturation (0.0–1.0) |
| `fc_errors_total` | Counter | Errors by domain and type |
| `fc_fetch_duration_seconds` | Histogram | Per-request latency |
| `fc_records_exported_total` | Counter | Records written to sink |
| `fc_worker_count` | Gauge | Active distributed workers |

A Grafana dashboard JSON is included in `infra/grafana/ferrocrawl.json`. Import it directly.

Start the full observability stack:

```bash
cd infra
docker compose up -d
```

---

## REST API

The control plane runs on port 8080 by default.

```
POST   /jobs              Submit seed URLs (returns job_id)
GET    /jobs/:id          Get job status and progress
DELETE /jobs/:id          Cancel a running job
GET    /frontier/stats    Queue depth, dedup stats
POST   /workers/register  Worker node registration
GET    /workers           List active workers
GET    /metrics           Prometheus scrape endpoint
GET    /health            Liveness probe
```

Example:

```bash
curl -X POST http://localhost:8080/jobs \
  -H "Content-Type: application/json" \
  -d '{
    "seeds": ["https://example.com"],
    "schema": "product",
    "depth": 4,
    "sink": "parquet"
  }'
```

---

## CLI Reference

```
ferrocrawl <COMMAND>

Commands:
  crawl     Start a crawl job (single-node or distributed coordinator)
  worker    Start a distributed worker node
  export    Convert or re-export an existing crawl output
  bench     Run the built-in benchmark suite
  help      Print this message

Options for `crawl`:
  --seed <URL>            Seed URL(s) to start from (repeatable)
  --schema <FILE>         Extraction schema TOML file
  --output <PATH>         Output path (file or directory)
  --concurrency <N>       Max concurrent requests [default: 200]
  --depth <N>             Max crawl depth [default: 3]
  --distributed           Enable distributed mode (requires --redis)
  --redis <URL>           Redis connection URL
  --config <FILE>         Config file [default: crawler.toml]
```

---

## Contributing

Contributions are welcome. Please open an issue before submitting a PR for significant changes.

```bash
# Run the full test suite
cargo test --workspace

# Run clippy (CI enforced)
cargo clippy --workspace --all-targets -- -D warnings

# Run benchmarks
cargo bench

# Check formatting
cargo fmt --check
```

The CI pipeline runs on every PR: `cargo test`, `cargo clippy`, `cargo fmt --check`, and the benchmark suite against minimum regressions.
---

## License

MIT — see [LICENSE](LICENSE).

---

<div align="center">
  <sub>Built with Rust, Tokio, and a healthy disrespect for slow crawlers.</sub>
</div>
