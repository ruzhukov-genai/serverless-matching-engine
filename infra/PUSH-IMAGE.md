# Push Docker Image to ECR

The sme-gateway Docker image has been built locally but needs to be pushed to ECR before the Lambda function can use it.

## Status
- **Image:** Built at `210352747749.dkr.ecr.us-east-1.amazonaws.com/serverless-matching-engine/sme-gateway:latest`
- **Issue:** The `openclaw-rz` IAM user doesn't have ECR push permissions (403 Forbidden)

## Option 1: Grant IAM Permissions (Recommended)

Add this policy to the `openclaw-rz` user or a role:

```json
{
    "Version": "2012-10-17",
    "Statement": [
        {
            "Effect": "Allow",
            "Action": [
                "ecr:GetAuthorizationToken",
                "ecr:BatchCheckLayerAvailability",
                "ecr:GetDownloadUrlForLayer",
                "ecr:PutImage",
                "ecr:InitiateLayerUpload",
                "ecr:UploadLayerPart",
                "ecr:CompleteLayerUpload"
            ],
            "Resource": "*"
        }
    ]
}
```

Then run:
```bash
aws ecr get-login-password --region us-east-1 | \
  docker login --username AWS --password-stdin 210352747749.dkr.ecr.us-east-1.amazonaws.com

docker push 210352747749.dkr.ecr.us-east-1.amazonaws.com/serverless-matching-engine/sme-gateway:latest
```

## Option 2: Assume a Role with ECR Permissions

If you have a role with ECR permissions:
```bash
aws sts assume-role --role-arn arn:aws:iam::210352747749:role/ROLE_NAME --role-session-name push-image

# Export returned credentials
export AWS_ACCESS_KEY_ID=...
export AWS_SECRET_ACCESS_KEY=...
export AWS_SESSION_TOKEN=...

# Then push
aws ecr get-login-password --region us-east-1 | \
  docker login --username AWS --password-stdin 210352747749.dkr.ecr.us-east-1.amazonaws.com

docker push 210352747749.dkr.ecr.us-east-1.amazonaws.com/serverless-matching-engine/sme-gateway:latest
```

## Option 3: Get Image from Local Docker

If you're on the same machine where the image was built:
```bash
docker images | grep sme-gateway
docker push 210352747749.dkr.ecr.us-east-1.amazonaws.com/serverless-matching-engine/sme-gateway:latest
```

## CloudFormation Note

The backend CloudFormation stack references the ECR image URI:
```
210352747749.dkr.ecr.us-east-1.amazonaws.com/serverless-matching-engine/sme-gateway:latest
```

If the image doesn't exist in ECR when Lambda tries to invoke, it will fail. You must push the image before the Lambda function is called.

## Verify Image in ECR

Once pushed, verify:
```bash
aws ecr describe-images --region us-east-1 --repository-name serverless-matching-engine/sme-gateway
```

---

All infrastructure is ready. Just need the Docker image pushed to proceed.
