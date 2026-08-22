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

  meta = {
    description = "Find, create and jump into project directories";
    homepage = "https://github.com/goofansu/try";
    mainProgram = "try";
    platforms = lib.platforms.unix;
  };
}
