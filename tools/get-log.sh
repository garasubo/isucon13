#!/bin/bash

set -euxo pipefail

SERVER=${SERVER:-isucon1}
DB_SERVER=${DB_SERVER:-isucon1}
ID=$(date +%H%M%S)

cd "$(dirname "$0")"

ssh "$SERVER" "sudo cat /var/log/nginx/access.log" | alp ltsv --sort=sum -r -m '/api/condition/[0-9a-z\-]+$,/api/isu/[0-9a-z\-]+$,/api/isu/[0-9a-z\-]+/icon,/isu/[0-9a-z\-]+$,/isu/[0-9a-z\-]+/condition,/isu/[0-9a-z\-]+/graph' > "../logs/access.$ID.log"
ssh "$DB_SERVER" "sudo mysqldumpslow -s t -t 20 /var/log/mysql/mariadb-slow.log" > "../logs/mariadb-slowdump.$ID.log"