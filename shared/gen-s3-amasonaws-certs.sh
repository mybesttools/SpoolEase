openssl s_client -connect s3.amazonaws.com:443 -showcerts </dev/null 2>/dev/null | sed -n '/-----BEGIN CERTIFICATE-----/,/-----END CERTIFICATE-----/p' > ./src/certs/s3.amazonaws.com.pem 
openssl x509 -in ./src/certs/s3.amazonaws.com.pem -text -noout > ./src/certs/s3.amazonaws.com.info
