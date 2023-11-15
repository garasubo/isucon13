#!/bin/bash

set -euxo pipefail

SERVER1=${SERVER:-isucon1}
SERVER2=${SERVER:-isucon2}
DB_SERVER=isucon1

cd "$(dirname "$0")/../"

## macでのcross compileがうまくいかないのでmacの時はrustのbuildをskip
if [[ "$OSTYPE" != "darwin"* ]]; then
pushd webapp/rust
cargo build --release
rsync -avr target/x86_64-unknown-linux-musl/release/isucondition "$SERVER1:/home/isucon/webapp/rust/target/release/"
rsync -avr target/x86_64-unknown-linux-musl/release/isucondition "$SERVER2:/home/isucon/webapp/rust/target/release/"
popd
fi
rsync -avr webapp/sql "$SERVER1:/home/isucon/webapp"
rsync -avr webapp/sql "$SERVER2:/home/isucon/webapp"

ssh "$SERVER1" "sudo rm -f /var/log/nginx/access.log"
ssh "$SERVER2" "sudo rm -f /var/log/nginx/access.log"
ssh "$DB_SERVER" "sudo rm -f /var/log/mysql/mariadb-slow.log"
ssh "$SERVER1" "sudo systemctl restart isucondition.rust.service"
ssh "$SERVER2" "sudo systemctl restart isucondition.rust.service"
ssh "$DB_SERVER" "sudo systemctl restart mariadb.service"
ssh "$SERVER1" "sudo systemctl restart nginx.service"
ssh "$SERVER2" "sudo systemctl restart nginx.service"
