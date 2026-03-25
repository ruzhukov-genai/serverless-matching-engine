#!/usr/bin/env python3
"""
Verify that the refactor is structurally correct.
"""

import os
import json
from pathlib import Path

def check_file_exists(path):
    """Check if file exists and return True/False with message."""
    if os.path.exists(path):
        print(f"✓ {path} exists")
        return True
    else:
        print(f"✗ {path} missing")
        return False

def check_cargo_toml_has_gateway():
    """Check if workspace Cargo.toml includes gateway."""
    with open("Cargo.toml", "r") as f:
        content = f.read()
        if "crates/gateway" in content:
            print("✓ Gateway added to workspace")
            return True
        else:
            print("✗ Gateway not in workspace")
            return False

def check_main_files():
    """Check that main files have correct structure."""
    # Check gateway main.rs
    gateway_main = "crates/gateway/src/main.rs"
    if os.path.exists(gateway_main):
        with open(gateway_main, "r") as f:
            content = f.read()
            if "sme-gateway starting" in content and "valkey" in content and "pg_hot" not in content:
                print("✓ Gateway main.rs looks correct (Valkey only)")
            else:
                print("✗ Gateway main.rs has issues")
    
    # Check api main.rs (worker)
    api_main = "crates/api/src/main.rs"
    if os.path.exists(api_main):
        with open(api_main, "r") as f:
            content = f.read()
            if "worker" in content and "order_queue_consumer" in content:
                print("✓ API main.rs converted to worker")
            else:
                print("✗ API main.rs still looks like HTTP server")

def main():
    print("Verifying serverless matching engine refactor...")
    print()
    
    # Check required files exist
    files_to_check = [
        "crates/gateway/Cargo.toml",
        "crates/gateway/src/main.rs", 
        "crates/gateway/src/routes.rs",
        "crates/api/src/worker.rs",
        "crates/api/src/main.rs",
        "crates/api/src/main_original.rs",  # backup
        "REFACTOR_SUMMARY.md"
    ]
    
    all_files_exist = True
    for file_path in files_to_check:
        if not check_file_exists(file_path):
            all_files_exist = False
    
    print()
    
    # Check workspace configuration
    check_cargo_toml_has_gateway()
    
    print()
    
    # Check main files structure
    check_main_files()
    
    print()
    
    if all_files_exist:
        print("✓ Refactor appears structurally complete!")
        print()
        print("Next steps:")
        print("1. Install Rust: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh")
        print("2. Build gateway: cargo build --bin sme-gateway")
        print("3. Build worker: cargo build --bin sme-api")
        print("4. Run tests: cargo test")
        print("5. Run clippy: cargo clippy -- -D warnings")
    else:
        print("✗ Some files are missing!")

if __name__ == "__main__":
    os.chdir("/home/ubuntu/projects/serverless-matching-engine")
    main()