# SPDX-License-Identifier: LGPL-3.0-or-later
# FreeBSD Ports skeleton for games/megu: dedicated server plus the optional
# Vulkan GPU client. Rust crates are vendored by the normal cargo.mk flow
# (`make makesum` regenerates CARGO_CRATES after a repocopy).

PORTNAME=	megu
DISTVERSIONPREFIX=	v
DISTVERSION=	0.1.0-alpha.3
CATEGORIES=	games net

MAINTAINER=	206304251+nekomario28@users.noreply.github.com
COMMENT=	Clean-room Minecraft-compatible server and native GPU client
WWW=		https://github.com/nekomario28/megu-mc

LICENSE=	LGPL3+
LICENSE_FILE=	${WRKSRC}/LICENSE

BUILD_DEPENDS=	rust>=1.98.0:lang/rust

USES=		cargo
USE_GITHUB=	yes
GH_ACCOUNT=	nekomario28
GH_PROJECT=	megu-mc

# The server binary is the root crate's `megu`; the installer and the GPU
# client are sibling path-crate binaries, so cargo.mk's single-binary flow
# is extended with explicit build/install targets below.
CARGO_FEATURES=	mod-suite create-offload

OPTIONS_DEFINE=		CLIENT_GPU
OPTIONS_DEFAULT=	CLIENT_GPU
OPTIONS_SUB=		yes

CLIENT_GPU_DESC=	Build the wgpu/winit Vulkan GPU game client
CLIENT_GPU_BUILD_DEPENDS=	vulkan-loader>0:graphics/vulkan-loader
CLIENT_GPU_LIB_DEPENDS=	libvulkan.so:graphics/vulkan-loader \
			libwayland-client.so:graphics/wayland \
			libxkbcommon.so:x11/libxkbcommon \
			libasound.so:audio/alsa-lib

PLIST_FILES=	bin/megu \
		bin/megu-installer \
		%%CLIENT_GPU%%bin/megu-client-gpu

post-patch:
	# Ports builds must not try to auto-update crates from the network.
	${REINPLACE_CMD} 's/^incremental = true/incremental = false/' \
		${WRKSRC}/Cargo.toml 2>/dev/null || true

do-build:
	cd ${WRKSRC} && ${SETENV} ${CARGO_ENV} ${CARGO_CARGO} build \
		--release --locked --features ${CARGO_FEATURES:ts,} --bin megu
	cd ${WRKSRC}/server-files && ${SETENV} ${CARGO_ENV} ${CARGO_CARGO} build \
		--release --locked --bin megu-installer
do-build-CLIENT_GPU-on:
	cd ${WRKSRC}/client-gpu && ${SETENV} ${CARGO_ENV} ${CARGO_CARGO} build \
		--release --locked --bin megu-client-gpu

do-install:
	${INSTALL_PROGRAM} ${WRKSRC}/target/release/megu ${STAGEDIR}${PREFIX}/bin
	${INSTALL_PROGRAM} ${WRKSRC}/server-files/target/release/megu-installer \
		${STAGEDIR}${PREFIX}/bin
do-install-CLIENT_GPU-on:
	${INSTALL_PROGRAM} ${WRKSRC}/client-gpu/target/release/megu-client-gpu \
		${STAGEDIR}${PREFIX}/bin

post-install:
	${MKDIR} ${STAGEDIR}${PREFIX}/share/licenses/${PORTNAME}
	${INSTALL_DATA} ${WRKSRC}/LICENSE ${WRKSRC}/LICENSING.md \
		${WRKSRC}/NOTICE ${STAGEDIR}${PREFIX}/share/licenses/${PORTNAME}

.include <bsd.port.mk>
