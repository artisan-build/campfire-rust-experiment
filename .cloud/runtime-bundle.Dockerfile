# syntax = docker/dockerfile:1
#
# Builds the libvips the Cloud build command links against and the app loads at run time.
#
# Cloud's Rust build image and its runtime container are both Debian 12 (bookworm, glibc 2.36) on
# arm64, and neither carries libvips or glib — so unlike the production Dockerfile, which builds
# libvips from the reference image's trixie sources, this builds the same libvips version against
# bookworm's codec libraries and ships the result plus its non-core shared-library closure.
#
# Two things differ from the production image's vips stage, and both are recorded as deliberate
# divergences in the report: the prefix, and -Dhighway=disabled, because bookworm ships libhwy
# 1.0.3 and libvips 8.16.1 wants >= 1.0.5. Highway is libvips' SIMD backend, so this costs speed,
# not loaders: the same formats load and save.

ARG DEBIAN_RELEASE=bookworm
ARG LIBVIPS_VERSION=8.16.1
ARG LIBVIPS_SHA256=d114d7c132ec5b45f116d654e17bb4af84561e3041183cd4bfd79abfb85cf724

FROM docker.io/library/debian:${DEBIAN_RELEASE}-slim AS build
ARG LIBVIPS_VERSION
ARG LIBVIPS_SHA256
RUN apt-get update -qq && apt-get install --no-install-recommends -y \
      ca-certificates curl xz-utils build-essential pkg-config meson ninja-build \
      libglib2.0-dev libexpat1-dev zlib1g-dev libjpeg62-turbo-dev libspng-dev libpng-dev \
      libwebp-dev libtiff-dev libheif-dev libexif-dev liblcms2-dev libcgif-dev libimagequant-dev
WORKDIR /usr/src
RUN curl -fsSLO "https://github.com/libvips/libvips/releases/download/v${LIBVIPS_VERSION}/vips-${LIBVIPS_VERSION}.tar.xz" && \
    echo "${LIBVIPS_SHA256}  vips-${LIBVIPS_VERSION}.tar.xz" | sha256sum -c - && \
    tar -xJf "vips-${LIBVIPS_VERSION}.tar.xz" && \
    cd "vips-${LIBVIPS_VERSION}" && \
    meson setup build --buildtype=plain --wrap-mode=nodownload --prefix=/opt/vips --libdir=lib \
      --auto-features=disabled -Dmodules=disabled -Dintrospection=disabled -Dcplusplus=false \
      -Ddeprecated=false -Dexamples=false \
      -Djpeg=enabled -Dspng=enabled -Dpng=enabled -Dwebp=enabled -Dtiff=enabled -Dheif=enabled \
      -Dexif=enabled -Dlcms=enabled -Dcgif=enabled -Dimagequant=enabled -Dhighway=disabled \
      -Dzlib=enabled && \
    meson install -C build --strip
