{
  lib,
  rustPlatform,
  sqlite,
}:
rustPlatform.buildRustPackage {
  pname = "chimera";
  version = "0.1.0";
  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [./Cargo.toml ./Cargo.lock ./crates];
  };
  cargoLock.lockFile = ./Cargo.lock;
  buildInputs = [sqlite];
  meta.mainProgram = "chimera";
}
