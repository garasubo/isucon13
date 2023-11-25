#!/bin/bash

set -euxo pipefail

SERVER=${SERVER:-isucon1}
DB_SERVER=${DB_SERVER:-isucon1}
ID=$(date +%H%M%S)

cd "$(dirname "$0")"

ssh "$SERVER" "sudo cat /var/log/nginx/access.log" | alp ltsv --sort=sum -r -m '/api/livestream/[0-9]+$,/api/livestream/[0-9]+/enter,/api/livestream/[0-9]+/statistics,/api/livestream/[0-9]+/moderate,/api/livestream/[0-9]+/livecomment,/api/livestream/[0-9]+/reaction,/api/livestream/[0-9]+/report,/api/livestream/[0-9]+/exit,/api/livestream/[0-9]+/ngwords,/api/user/[a-zA-Z0-9]+/icon,/api/user/[a-zA-Z0-9]+/statistics,/api/user/[a-zA-Z0-9]+/theme' > "../logs/access.$ID.log"
ssh "$DB_SERVER" "sudo mysqldumpslow -s t -t 20 /var/log/mysql/mysql-slow.log" > "../logs/mysql-slowdump.$ID.log"