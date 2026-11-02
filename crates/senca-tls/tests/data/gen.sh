#!/usr/bin/env bash
# 試験用の証明書を作り直す（OpenSSL）。鍵は試験専用で秘密ではない。
set -euo pipefail
cd "$(dirname "$0")"
export OPENSSL_CONF=/dev/null
cat > ext.cnf <<'CNF'
[req]
distinguished_name=dn
[dn]
[v3_root]
basicConstraints=critical,CA:TRUE
keyUsage=critical,keyCertSign,cRLSign
subjectKeyIdentifier=hash
[v3_inter]
basicConstraints=critical,CA:TRUE,pathlen:0
keyUsage=critical,keyCertSign,cRLSign
subjectKeyIdentifier=hash
authorityKeyIdentifier=keyid
[v3_server]
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature
extendedKeyUsage=serverAuth
subjectAltName=DNS:localhost,DNS:example.test,DNS:*.wild.test,IP:127.0.0.1
authorityKeyIdentifier=keyid
[v3_client]
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature
extendedKeyUsage=clientAuth
subjectAltName=DNS:node1.senca.internal
authorityKeyIdentifier=keyid
CNF
ec() { openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out "$1"; }
ec ca.key
openssl req -config ext.cnf -x509 -new -key ca.key -subj "/CN=SENCA Test Root" -days 36500 -sha256 -extensions v3_root -out ca.pem
ec inter.key
openssl req -config ext.cnf -new -key inter.key -subj "/CN=SENCA Test Intermediate" -out t.csr
openssl x509 -req -in t.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 36500 -sha256 -extfile ext.cnf -extensions v3_inter -out inter.pem
ec server-ecdsa.key
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out server-rsa.key
openssl genpkey -algorithm ED25519 -out server-ed25519.key
for k in ecdsa rsa ed25519; do
  openssl req -config ext.cnf -new -key "server-$k.key" -subj "/CN=localhost" -out t.csr
  openssl x509 -req -in t.csr -CA inter.pem -CAkey inter.key -CAcreateserial -days 36500 -sha256 -extfile ext.cnf -extensions v3_server -out t.crt
  cat t.crt inter.pem > "server-$k.pem"
done
ec client.key
openssl req -config ext.cnf -new -key client.key -subj "/CN=node1" -out t.csr
openssl x509 -req -in t.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 36500 -sha256 -extfile ext.cnf -extensions v3_client -out client.pem
# 期限切れのサーバー証明書（鍵は server-ecdsa.key）
openssl req -config ext.cnf -new -key server-ecdsa.key -subj "/CN=localhost" -out t.csr
openssl x509 -req -in t.csr -CA ca.pem -CAkey ca.key -CAcreateserial -not_before 20200101000000Z -not_after 20210101000000Z -sha256 -extfile ext.cnf -extensions v3_server -out expired.pem
# 信頼しない別の root
ec other.key
openssl req -config ext.cnf -x509 -new -key other.key -subj "/CN=Other Root" -days 36500 -sha256 -extensions v3_root -out other-ca.pem
rm -f t.csr t.crt ./*.srl ext.cnf other.key ca.key inter.key
