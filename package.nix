{
  lib,
  rustPlatform,
  version ? "0.1.0",
}:

rustPlatform.buildRustPackage {
  pname = "try";
  inherit version;

  # Only the sources that affect the build, so editing the README does not
  # trigger a rebuild.
  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [
      ./Cargo.toml
      ./Cargo.lock
      ./src
    ];
  };

  cargoLock.lockFile = ./Cargo.lock;

  # The vendored crate sources sit under the per-build directory, whose name
  # carries the builder's pid, and rustc bakes those absolute paths into the
  # binary for panic messages. Remap the prefix so two builds agree byte for
  # byte, and so the binary does not name a directory that no longer exists.
  preBuild = ''
    export RUSTFLAGS="--remap-path-prefix=$NIX_BUILD_TOP=/build $RUSTFLAGS"
  '';

  meta = {
    description = "Find, create and jump into project directories";
    homepage = "https://github.com/goofansu/try";
    mainProgram = "try";
    platforms = lib.platforms.unix;
  };
}
