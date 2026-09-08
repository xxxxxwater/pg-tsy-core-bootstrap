# Security

This is trading infrastructure. Treat credentials, account identifiers, position data and production topology as sensitive.

- Store secrets in AWS Secrets Manager or local environment variables, never Git.
- Use read-only credentials for research/data collectors when possible.
- Separate research and execution IAM/security groups.
- Production order credentials must only be mounted into the live execution host/process.
- Rotate credentials after any suspected exposure.
- Redact exchange payloads before attaching logs to public issues or AI prompts.
