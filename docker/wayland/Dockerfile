# Headless Wayland session for voxl's portal capture path.
FROM ubuntu:26.04

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential pkg-config clang libclang-dev curl ca-certificates \
        libx264-dev libyuv-dev libopus-dev libpulse-dev \
        libpipewire-0.3-dev \
        rustc cargo \
        esbuild \
        dbus dbus-x11 \
        pipewire \
        wireplumber \
        labwc \
        wlr-randr \
        xdg-desktop-portal \
        xdg-desktop-portal-wlr \
        libgtk-3-dev \
        python3 python3-pil \
        seatd \
    && mkdir -p /usr/lib/x86_64-linux-gnu/pkgconfig \
    && printf '%s\n' \
        'prefix=/usr' \
        'libdir=${prefix}/lib/x86_64-linux-gnu' \
        'includedir=${prefix}/include' \
        '' \
        'Name: libyuv' \
        'Description: YUV conversion and scaling' \
        'Version: 0' \
        'Libs: -L${libdir} -lyuv' \
        'Cflags: -I${includedir}' \
        > /usr/lib/x86_64-linux-gnu/pkgconfig/libyuv.pc \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
CMD ["bash", "docker/wayland/test.sh"]
