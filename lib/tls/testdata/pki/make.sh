#!/bin/sh
# Generates the test PKI of vtls' handshake tests with OpenSSL 3.4 or newer:
#
#   sh make.sh        (in lib/tls/testdata/pki)
#
# Every certificate is valid from 2026-01-01 to 2036-01-01 unless noted, and
# the tests check them at 2026-06-01. The private keys are test keys, stored
# as DER (SEC1 for ECDSA, PKCS#8 for Ed25519, PKCS#1 for RSA).
set -e
# Git Bash would turn "/CN=..." into a Windows path.
export MSYS_NO_PATHCONV=1
NB=20260101000000Z
NA=20360101000000Z
NAME=test.veda.local

cat > ext-ca.cnf <<EOF
basicConstraints = critical, CA:TRUE
keyUsage = critical, keyCertSign, cRLSign
subjectKeyIdentifier = hash
authorityKeyIdentifier = keyid
EOF
cat > ext-leaf.cnf <<EOF
basicConstraints = critical, CA:FALSE
keyUsage = critical, digitalSignature, keyEncipherment
extendedKeyUsage = serverAuth
subjectAltName = DNS:$NAME, DNS:localhost, IP:127.0.0.1
EOF
cat > ext-other-name.cnf <<EOF
basicConstraints = critical, CA:FALSE
keyUsage = critical, digitalSignature
extendedKeyUsage = serverAuth
subjectAltName = DNS:other.veda.local
EOF
cat > ext-client-only.cnf <<EOF
basicConstraints = critical, CA:FALSE
keyUsage = critical, digitalSignature
extendedKeyUsage = clientAuth
subjectAltName = DNS:$NAME
EOF

key() { # NAME ALGORITHM [OPTION]
    if [ -n "$3" ]; then
        openssl genpkey -algorithm "$2" -pkeyopt "$3" -out "$1.pem"
    else
        openssl genpkey -algorithm "$2" -out "$1.pem"
    fi
}
root() { # NAME SUBJECT
    openssl req -x509 -new -key "$1.pem" -subj "/CN=$2" -sha256 -not_before $NB -not_after $NA \
        -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" -out "$1.crt"
}
issue() { # NAME KEY SUBJECT ISSUER EXTENSIONS NOT_BEFORE NOT_AFTER SIGNING-OPTIONS...
    name=$1 key=$2 subject=$3 issuer=$4 ext=$5 nb=$6 na=$7
    shift 7
    openssl req -new -key "$key.pem" -subj "/CN=$subject" -out "$name.csr"
    openssl x509 -req -in "$name.csr" -CA "$issuer.crt" -CAkey "$issuer.pem" -set_serial "0x$(openssl rand -hex 8)" \
        -not_before "$nb" -not_after "$na" -extfile "$ext" "$@" -out "$name.crt"
}

key ca-ecdsa EC ec_paramgen_curve:P-256
key ca-rsa RSA rsa_keygen_bits:2048
key untrusted-ca EC ec_paramgen_curve:P-256
key int-ecdsa EC ec_paramgen_curve:P-384
key leaf-p256 EC ec_paramgen_curve:P-256
key leaf-p384 EC ec_paramgen_curve:P-384
key leaf-ed25519 ED25519
key leaf-rsa RSA rsa_keygen_bits:2048

root ca-ecdsa "Veda Test ECDSA Root"
root ca-rsa "Veda Test RSA Root"
root untrusted-ca "Veda Untrusted Root"
issue int-ecdsa int-ecdsa "Veda Test ECDSA Intermediate" ca-ecdsa ext-ca.cnf $NB $NA -sha256

# Signature algorithms of the certificates (issuer key, hash):
issue leaf-p256 leaf-p256 $NAME ca-ecdsa ext-leaf.cnf $NB $NA -sha256              # P-256, SHA-256
issue leaf-p384 leaf-p384 $NAME ca-ecdsa ext-leaf.cnf $NB $NA -sha384              # P-256, SHA-384
issue leaf-chain leaf-p256 $NAME int-ecdsa ext-leaf.cnf $NB $NA -sha384            # P-384, SHA-384
issue leaf-ed25519 leaf-ed25519 $NAME int-ecdsa ext-leaf.cnf $NB $NA -sha256       # P-384, SHA-256
issue leaf-rsa leaf-rsa $NAME ca-rsa ext-leaf.cnf $NB $NA -sha384                  # RSA PKCS#1, SHA-384
issue leaf-rsa-pss leaf-rsa $NAME ca-rsa ext-leaf.cnf $NB $NA -sha256 \
    -sigopt rsa_padding_mode:pss -sigopt rsa_pss_saltlen:32 -sigopt rsa_mgf1_md:sha256  # RSA-PSS, SHA-256

# Certificates the client must reject.
issue leaf-expired leaf-p256 $NAME ca-ecdsa ext-leaf.cnf 20240101000000Z 20250101000000Z -sha256
issue leaf-future leaf-p256 $NAME ca-ecdsa ext-leaf.cnf 20300101000000Z 20310101000000Z -sha256
issue leaf-other-name leaf-p256 other.veda.local ca-ecdsa ext-other-name.cnf $NB $NA -sha256
issue leaf-untrusted leaf-p256 $NAME untrusted-ca ext-leaf.cnf $NB $NA -sha256
issue leaf-client-only leaf-p256 $NAME ca-ecdsa ext-client-only.cnf $NB $NA -sha256

for c in ca-ecdsa ca-rsa int-ecdsa leaf-p256 leaf-p384 leaf-chain leaf-ed25519 leaf-rsa leaf-rsa-pss \
    leaf-expired leaf-future leaf-other-name leaf-untrusted leaf-client-only; do
    openssl x509 -in "$c.crt" -outform DER -out "$c.der"
done
openssl ec -in leaf-p256.pem -outform DER -out leaf-p256.key.der
openssl ec -in leaf-p384.pem -outform DER -out leaf-p384.key.der
openssl pkey -in leaf-ed25519.pem -outform DER -out leaf-ed25519.key.der
openssl rsa -in leaf-rsa.pem -traditional -outform DER -out leaf-rsa.key.der

rm -f ./*.pem ./*.crt ./*.csr ./*.cnf ./*.srl
