# Prepared immutable base; execution builds require no network or package install.
FROM bash:5.2.37@sha256:3bee76a96d86d5d2d5efc7c1c570e5a7c95db22348a26944e0e546fa174e3324
COPY run.sh /fixture/run.sh
USER 65532:65532
ENTRYPOINT ["bash", "/fixture/run.sh"]
