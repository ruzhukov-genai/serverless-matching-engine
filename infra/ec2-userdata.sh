#!/bin/bash
# =============================================================================
# ec2-userdata.sh — DEPRECATED
#
# EC2 previously ran Dragonfly (Redis-compatible cache). This has been replaced
# by ElastiCache Valkey. The EC2 instance is no longer needed and can be removed
# from the CloudFormation stack.
#
# All services are now managed:
# - PostgreSQL → RDS (db.t4g.small)
# - Redis cache → ElastiCache Valkey (cache.t4g.micro)
# - Order processing → Lambda (worker)
# - HTTP/WS API → Lambda (gateway) + API Gateway
# =============================================================================

echo "This EC2 instance is deprecated. All services run on managed AWS resources."
