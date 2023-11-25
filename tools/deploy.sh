#!/bin/bash

set -euxo pipefail

SERVER1=${SERVER:-isucon1}
SERVER2=${SERVER:-isucon2}
SERVER3=${SERVER:-isucon3}
DB_SERVER=isucon1

cd "$(dirname "$0")/../"

## macでのcross compileがうまくいかないのでmacの時はrustのbuildをskip
if [[ "$OSTYPE" != "darwin"* ]]; then
pushd webapp/rust
cargo build --release
rsync -avr target/release/isupipe "$SERVER1:/home/isucon/webapp/rust/target/release/"
popd
fi
rsync -avr webapp/sql "$SERVER1:/home/isucon/webapp"

ssh "$SERVER1" "sudo rm -f /var/log/nginx/access.log"
ssh "$DB_SERVER" "sudo rm -f /var/log/mysql/mysql-slow.log"
ssh "$SERVER1" "sudo systemctl restart isupipe-rust.service"
ssh "$DB_SERVER" "sudo systemctl restart mysql.service"
ssh "$SERVER1" "sudo systemctl restart nginx.service"
