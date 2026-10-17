ARG BASE_IMAGE=python@sha256:34386ef0cb081344d7ec1c103ba398e6e9f64e9ab3a1509accc92a4e24a07258
FROM ${BASE_IMAGE}

ARG TOOLSETS=core,computation,browser,systems
ENV PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1 \
    PLAYWRIGHT_BROWSERS_PATH=/opt/browsers \
    MPLCONFIGDIR=/tmp/matplotlib \
    XDG_CACHE_HOME=/tmp/cache \
    HOME=/work \
    LANG=C.UTF-8 \
    TZ=UTC

COPY requirements-*.lock /opt/workshop/
COPY install.sh /opt/workshop/install.sh
RUN /bin/sh /opt/workshop/install.sh "$TOOLSETS"
COPY tool.py /opt/playtime/tool.py
COPY docs/ /opt/workshop/docs/
RUN mkdir -p /work && chown 1000:1000 /work
WORKDIR /work
# The controller runs tools as uid 1000. The root-owned lifetime timer is not
# signalable by workshop programs; all Linux capabilities are dropped at run time.
CMD ["sleep", "infinity"]
