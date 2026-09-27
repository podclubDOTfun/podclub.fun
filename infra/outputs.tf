output "cloudfront_domain" {
  value       = aws_cloudfront_distribution.site.domain_name
  description = "CloudFront distribution domain (the public edge). Point DNS here / used by the alias record."
}

output "site_url" {
  value       = "https://${var.domain_name}"
  description = "Public beta URL (Basic-Auth gated at the edge)."
}

output "origin_bucket" {
  value       = aws_s3_bucket.site.bucket
  description = "Private S3 origin bucket to sync the built web/ files into (aws s3 sync)."
}

output "distribution_id" {
  value       = aws_cloudfront_distribution.site.id
  description = "Distribution id — use for cache invalidation after each deploy."
}
