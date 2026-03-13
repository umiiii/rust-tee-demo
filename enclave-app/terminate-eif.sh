CID=4

EnclaveID=$(nitro-cli describe-enclaves | jq -r --arg cid "$CID" '.[] | select(.EnclaveCID == ($cid | tonumber)) | .EnclaveID')

nitro-cli terminate-enclave --enclave-id $EnclaveID