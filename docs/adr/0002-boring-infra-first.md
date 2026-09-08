# ADR-0002: Boring infrastructure first

Status: Accepted

## Decision

Use S3, Parquet/Arrow, PostgreSQL and dedicated EC2 before considering Kafka, Kubernetes or custom distributed systems.

## Reason

The dominant early risks are trading-state correctness, reconciliation and research validity—not service orchestration scalability.
