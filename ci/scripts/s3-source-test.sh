#!/usr/bin/env bash

set -euo pipefail

source ci/scripts/common.sh

while getopts 'p:s:' opt; do
    case ${opt} in
        p )
            profile=$OPTARG
            ;;
        s )
            script=$OPTARG
            ;;
        \? )
            echo "Invalid Option: -$OPTARG" 1>&2
            exit 1
            ;;
        : )
            echo "Invalid option: $OPTARG requires an argument" 1>&2
            ;;
    esac
done
shift $((OPTIND -1))

download_and_prepare_rw "$profile" source

echo "--- starting risingwave cluster with connector node"
cargo make ci-start ci-1cn-1fe

echo "--- Run test"
python3 -m pip install minio psycopg2-binary
python3 e2e_test/s3/$script

echo "--- Kill cluster"
cargo make ci-kill
