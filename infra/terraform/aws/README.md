# AWS deployment blueprint

The first production topology should be intentionally small:

- `research`: EC2/AWS Batch jobs with S3 access; GPU instances started only for training.
- `live-core`: dedicated EC2, minimal inbound network surface, systemd or one controlled container.
- `state`: RDS PostgreSQL.
- `data`: S3 with lifecycle policies.
- `secrets`: Secrets Manager.
- `observability`: CloudWatch + Prometheus/Grafana.

Terraform in this directory starts as a safe skeleton. Do not `apply` production networking or RDS changes without reviewing cost, region, backups, encryption and recovery objectives.
