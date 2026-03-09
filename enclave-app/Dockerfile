FROM scratch

COPY --chown=0:0 --chmod=744 ./enclave /enclave

CMD ["/enclave" , "-contract", "/contract.json"]
