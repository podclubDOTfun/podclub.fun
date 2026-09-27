# podclub.fun — private-beta web hosting (AWS)

Static site → **private S3 origin** → **CloudFront** (public edge, HTTPS) → served only
to visitors who pass a **shared access code** enforced by a CloudFront Function at the
viewer-request edge. No owner server, no owner IP, no third-party tracking.

This maps directly to the privacy rules for the launchpad:

- **Owner IP never exposed** — the only origin is a private S3 bucket reached over
  CloudFront OAC (SigV4). There is no EC2/owner box in the request path.
- **CloudFront is the public layer** — the bucket blocks all public access; only the
  distribution can read it.
- **Custom domain over HTTPS** — ACM cert (us-east-1) + `redirect-to-https`, TLS 1.2+.
- **No secrets in the repo** — the access code is a `sensitive` Terraform var supplied
  via `TF_VAR_access_code` or a gitignored `*.tfvars`; it is base64-embedded into the
  edge function only at deploy time.
- **No 3rd-party tracking** — nothing here adds analytics; the site stays client-side.

## Files

- `main.tf` — S3 (private) + OAC + bucket policy, CloudFront Function gate, ACM cert
  with Route53 DNS validation, CloudFront distribution, Route53 alias record.
- `variables.tf` / `outputs.tf` — inputs and the URLs/ids you get back.
- `edge/gate.js.tftpl` — the shared-code Basic-Auth function (templated with the code).
- `terraform.tfvars.example` — copy to `terraform.tfvars` (gitignored) and fill in.

## Prerequisites (not in the base build environment)

1. **Terraform** and the **AWS CLI** installed on whatever box runs the deploy
   (the base environment has neither — run this from the EC2 build box in `../scripts/ec2-build-bootstrap.sh`
   or any machine with AWS credentials).
2. An **AWS account** + credentials (`aws configure` / env vars). Least-privilege is fine:
   S3, CloudFront, ACM, Route53.
3. A **domain** with a **Route53 hosted zone** (`hosted_zone_id`). If DNS is elsewhere,
   validate the ACM cert manually instead of via the Route53 records here.
4. The **shared beta code** — decided out-of-band, never committed.

## Deploy (stage by stage — verify each before the next)

```sh
cd fast-launch/infra
cp terraform.tfvars.example terraform.tfvars   # fill domain_name + hosted_zone_id
export TF_VAR_access_code='<the shared beta code>'   # not in any file

terraform init
terraform plan     # STAGE CHECK: review every resource; confirm no public-read on the bucket
terraform apply    # creates cert (DNS-validated), function, distribution, alias

# build + publish the site (from repo root, after the web build step):
aws s3 sync ../web "s3://$(terraform output -raw origin_bucket)/" --delete
aws cloudfront create-invalidation \
  --distribution-id "$(terraform output -raw distribution_id)" --paths '/*'
```

**Stage checks:**

1. After `apply`: `curl -I https://<domain>` returns **401** with no credentials.
2. `curl -I -u beta:<code> https://<domain>` returns **200**.
3. Confirm the S3 bucket is not reachable directly (public access block on).
4. Rotating the code = change `TF_VAR_access_code`, `terraform apply` (republishes the
   function), then re-check 1–2. Old code stops working immediately.

## Notes

- The gate is a **shared code**, appropriate for a private beta — it is not per-user auth.
  Anyone with the code gets in; treat it as a soft wall, not identity.
- Cache: the 401 responses set `no-store`; static assets use CloudFront's managed
  CachingOptimized policy. Always invalidate `/*` after a sync.
