#!/bin/bash

set -euo pipefail

# MSRV
msrv="1.70.0"

is_ci=false
is_msrv=false
version=""

# Check if "msrv" is passed as an argument
if [[ "$#" -gt 0 && "$1" == "msrv" ]]; then
    is_msrv=true
    version="+$msrv"
fi

# Check if "ci" is passed as an argument
if [[ "$#" -gt 0 && "$2" == "ci" ]]; then
    is_ci=true
fi

# Check if MSRV
if [ "$is_msrv" == true ]; then
    # Install MSRV
    rustup install $msrv
    rustup component add clippy --toolchain $msrv
    rustup target add wasm32-unknown-unknown --toolchain $msrv
fi

echo "CI: $is_ci"
echo "MSRV: $is_msrv"

buildargs=(
    "-p nostr"
    "-p nostr --no-default-features --features alloc"
    "-p nostr --no-default-features --features alloc,all-nips"
    "-p nostr-database"
    "-p nostr-lmdb"
    "-p nostr-relay-builder"
    "-p nostr-sdk"
    "-p nostr-sdk --no-default-features"
    "-p nostr-sdk --features nip47,nip57"
    "-p nostr-sdk --features nip47,nip57 --target wasm32-unknown-unknown"
    "-p nostr-sdk --features indexeddb,webln --target wasm32-unknown-unknown"
    "-p nostr-sdk --features lmdb"
    "-p nostr-sdk --features ndb"
    "-p nostr-sdk --features tor"
    "-p nostr-cli"
)

skip_msrv=(
    "-p nostr-cli"     # MSRV: 1.74.0
)

for arg in "${buildargs[@]}";
do
    # Skip the current crate if is_msrv is true and it's in the skip list
    skip=false
    for skip_arg in "${skip_msrv[@]}";
    do
        if [ "$is_msrv" == true ] && [[ "$arg" == "$skip_arg" ]]; then
            skip=true
            break
        fi
    done
    if [ "$skip" == true ]; then
        echo "Skipping MSRV check for '$arg'"
        echo
        continue
    fi

    if [[ $version == "" ]];
    then
        echo  "Checking '$arg' [default]"
    else
        echo  "Checking '$arg' [$version]"
    fi

    cargo $version check $arg

    if [[ $arg != *"--target wasm32-unknown-unknown"* ]];
    then
        cargo $version test $arg
    fi

    cargo $version clippy $arg -- -D warnings

    # If CI, clean every time to avoid to go out of space (GitHub Actions issue)
    if [ "$is_ci" == true ]; then
        cargo clean
    fi

    echo
done
