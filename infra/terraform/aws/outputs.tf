output "data_bucket" { value = aws_s3_bucket.data.bucket }
output "live_core_log_group" { value = aws_cloudwatch_log_group.live_core.name }
