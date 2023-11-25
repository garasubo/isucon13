# isucon13
isucon13本選用レポジトリ

[ポータル](https://portal.isucon.net/dashboard/)

[スプレッドシート](https://docs.google.com/spreadsheets/d/12kknFZRLcVEQFisFzYmrZGlpUiF2w-PPw9GSFlaJcbw/edit#gid=0)

[当日マニュアル](https://gist.github.com/kazeburo/bccc2d2b2b9dc307b5640ae855f3e0bf)

[アプリケーションマニュアル](https://gist.github.com/kazeburo/70b352e6d51969b214f919bcf0794ba6)

## 当日までにやること
- [ ] お昼ご飯の用意
- [ ] このリポジトリをcloneしておく

## 当日最初にやること
- [ ] cloudformationの実行
- [ ] ansibleを実行
- [ ] ソースコードのコピー
- [ ] nginx・mysqlのログ設定
- [ ] 言語実装の切り替え
- [ ] ベンチマーク実行
- [ ] データベースを外部アクセス可能にする
- [ ] データベースアクセスパターン洗い出し

### MySQLの外部アクセス許可について
MySQL 8の場合、まず、`mysqld.conf`を以下のように編集する
- `bind-address`をコメントアウト
- `skip-grant-tables`を追加

次に`mysql -u root`でログインし、以下のクエリを実行する（ユーザー名やパスワードは適宜変える）
```sql
FLUSH PRIVILEGES;
CREATE USER 'isucon'@'%' IDENTIFIED BY 'isucon';
GRANT ALL PRIVILEGES ON *.* TO 'isucon'@'%';
```

その後、再度`mysqld.conf`を編集し、`skip-grant-tables`をコメントアウトする

## 本番終了30分前にやること
- [ ] ログ設定の停止
- [ ] 再起動試験
- [ ] ベンチマークガチャ