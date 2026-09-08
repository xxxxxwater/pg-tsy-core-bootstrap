provider "aws" { region = var.aws_region }

locals { name = "${var.project}-${var.environment}" }

resource "aws_s3_bucket" "data" {
  bucket_prefix = "${local.name}-data-"
}

resource "aws_s3_bucket_versioning" "data" {
  bucket = aws_s3_bucket.data.id
  versioning_configuration { status = "Enabled" }
}

resource "aws_cloudwatch_log_group" "live_core" {
  name              = "/${local.name}/live-core"
  retention_in_days = 30
}
