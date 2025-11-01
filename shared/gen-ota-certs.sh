openssl s_client -connect raw.githubusercontent.com:443 -showcerts </dev/null 2>/dev/null | sed -n '/-----BEGIN CERTIFICATE-----/,/-----END CERTIFICATE-----/p' > ./src/certs/raw.githubusercontent.com.pem
openssl x509 -in ./src/certs/raw.githubusercontent.com.pem -text -noout > ./src/certs/raw.githubusercontent.com.info

openssl s_client -connect bin.spoolease.io:443 -showcerts </dev/null 2>/dev/null | sed -n '/-----BEGIN CERTIFICATE-----/,/-----END CERTIFICATE-----/p' > ./src/certs/bin.spoolease.io.pem
openssl x509 -in ./src/certs/bin.spoolease.io.pem -text -noout > ./src/certs/bin.spoolease.io.info

echo IMPORTANT: leave only the last part of the pem files (the CA)
