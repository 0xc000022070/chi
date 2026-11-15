{
  description = "What chi runs after each build of this repo";

  inputs = {
    chi.url = "github:0xc000022070/chi";
    nixpkgs.follows = "chi/nixpkgs";
  };

  outputs = {
    chi,
    nixpkgs,
    ...
  }: let
    systems = nixpkgs.lib.genAttrs ["x86_64-linux" "aarch64-linux"];
  in {
    packages = systems (system: let
      pkgs = nixpkgs.legacyPackages.${system};
      pipeline = chi.lib.pipeline pkgs;
    in {
      chi-pipeline = pipeline.mkPipeline {
        steps = [
          (pipeline.steps.releaseNotes {})
          {
            name = "checksums";
            runtimeInputs = [pkgs.coreutils];
            script = ''
              cd "$CHI_IN/artifacts"
              mkdir -p "$CHI_WORK/artifacts"
              sha256sum -- * > "$CHI_WORK/artifacts/SHA256SUMS"
            '';
          }
        ];
      };
    });
  };
}
