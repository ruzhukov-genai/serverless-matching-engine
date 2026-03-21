#!/bin/bash
# Deploy Worker Lambda image and update CloudFormation stack
# Usage: ./deploy-worker.sh

set -e

ACCOUNT=210352747749
REGION=us-east-1
WORKER_REPO=serverless-matching-engine/sme-worker
WORKER_ECR="${ACCOUNT}.dkr.ecr.${REGION}.amazonaws.com/${WORKER_REPO}"
STACK_NAME=serverless-matching-engine-backend

echo "=== Worker Lambda Deployment ==="
echo ""

# Step 1: Authenticate with ECR
echo "📦 Authenticating with ECR..."
aws ecr get-login-password --region $REGION | \
  docker login --username AWS --password-stdin $ACCOUNT.dkr.ecr.$REGION.amazonaws.com

# Step 2: Build Docker image (ARM64)
echo "🔨 Building Worker Lambda Docker image (ARM64)..."
echo "   This may take 15-20 minutes on first build (cargo + cross-compilation)"
docker buildx build \
  --platform linux/arm64 \
  -f infra/Dockerfile.worker \
  -t $WORKER_ECR:latest \
  --provenance=false \
  --sbom=false \
  -o type=docker \
  .

echo "✅ Docker image built locally"

# Step 3: Tag and push to ECR
echo "📤 Pushing to ECR..."
docker tag $WORKER_ECR:latest $WORKER_ECR:latest
docker push $WORKER_ECR:latest

# Get the image URI (digest)
WORKER_IMAGE_URI="$WORKER_ECR@$(docker inspect --format='{{index .RepoDigests 0}}' $WORKER_ECR:latest | cut -d'@' -f2)"

echo "✅ Pushed to ECR"
echo "   Image URI: $WORKER_IMAGE_URI"
echo ""

# Step 4: Update CloudFormation stack
echo "🚀 Updating CloudFormation stack..."
aws cloudformation update-stack \
  --stack-name $STACK_NAME \
  --template-body file://infra/cfn-backend.yaml \
  --parameters \
    ParameterKey=WorkerImageUri,ParameterValue="$WORKER_IMAGE_URI" \
  --capabilities CAPABILITY_NAMED_IAM \
  --region $REGION

echo "✅ Stack update initiated"
echo ""
echo "Monitor stack status:"
echo "  aws cloudformation describe-stacks --stack-name $STACK_NAME --region $REGION"
echo ""
