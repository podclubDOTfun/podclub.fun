variable "project" {
  type        = string
  default     = "fastlaunch"
  description = "Name prefix for all resources."
}

variable "domain_name" {
  type        = string
  description = "Fully-qualified domain the private beta is served on, e.g. beta.fastlaunch.app."
}

variable "hosted_zone_id" {
  type        = string
  description = "Route53 hosted zone id that owns domain_name (used for ACM DNS validation + the alias record)."
}

variable "access_user" {
  type        = string
  default     = "beta"
  description = "Basic-auth username half of the shared beta credential. Not secret on its own."
}

variable "access_code" {
  type        = string
  sensitive   = true
  description = "Shared beta access code (the password half). Provide via a gitignored *.tfvars file or TF_VAR_access_code — never commit it."
}

variable "tags" {
  type        = map(string)
  default     = { project = "fastlaunch", tier = "private-beta" }
  description = "Tags applied to every resource."
}
