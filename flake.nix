{
  description = "chi: self-hosted issue tracker, error tracking, builds and agents";

  inputs.nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";

  outputs = {
    self,
    nixpkgs,
  }: let
    systems = nixpkgs.lib.genAttrs ["aarch64-linux" "x86_64-linux"];
    pkgsFor = system: nixpkgs.legacyPackages.${system};
  in {
    packages = systems (system: {
      default = (pkgsFor system).callPackage ./package.nix {};
    });

    lib.pipeline = import ./nix/pipeline.nix;

    nixosModules.default = import ./nix/module.nix self;

    templates.pipeline = {
      path = ./templates/pipeline;
      description = "A repo flake exporting chi-pipeline";
    };

    devShells = systems (system: let
      pkgs = pkgsFor system;
    in {
      default = pkgs.mkShell {
        inputsFrom = [self.packages.${system}.default];
        packages = [pkgs.clippy pkgs.rustfmt pkgs.rust-analyzer pkgs.sqlite pkgs.bubblewrap pkgs.jq pkgs.gdrive3 pkgs.alejandra];
      };
    });

    formatter = systems (system: (pkgsFor system).alejandra);

    checks = systems (system: let
      pkgs = pkgsFor system;
      pipeline = self.lib.pipeline pkgs;
      notes = pipeline.mkPipeline {steps = [(pipeline.steps.notesFormat {})];};
    in {
      package = self.packages.${system}.default;

      pipeline = pipeline.mkPipeline {
        steps = [
          (pipeline.steps.releaseNotes {})
          (pipeline.steps.driveUpload {tags = ["release"];})
          (pipeline.steps.apkVariants {
            variants = [
              {
                suffix = "qa-b";
                package = "com.example.qab";
                label = "Example QA B";
              }
            ];
          })
        ];
      };

      notes-format = pkgs.runCommand "chi-notes-format" {} ''
        export CHI_WORK=$PWD
        printf '%s\n' '- uno' '===KNOWN_BUGS===' 'nada' '===TECHNICAL===' '- detalle' > notes-raw.txt
        ${notes}/bin/notes-format
        diff notes.md - <<'EOF'
        ## Cambios
        - uno

        ## Errores conocidos
        nada

        ## Técnico
        - detalle
        EOF
        touch $out
      '';

      service = pkgs.testers.runNixOSTest {
        name = "chi";
        nodes.machine = {
          imports = [self.nixosModules.default];
          services.chi = {
            enable = true;
            url = "http://machine:8080";
            agents = [];
            githubSecretFile = pkgs.writeText "github-secret" "hush";
          };
        };
        testScript = ''
          machine.wait_for_unit("chi.service")
          machine.wait_for_open_port(8080)
          machine.succeed("curl -sf http://127.0.0.1:8080/login")
          machine.wait_until_succeeds("journalctl -u chi | grep -q 'create one at'")
          assert machine.succeed("chi-agent-login id -un").strip() == "chi"
          code = machine.succeed("curl -s -o /dev/null -w '%{http_code}' -X POST -d '{}' http://127.0.0.1:8080/api/github")
          assert code == "401", f"unsigned webhook answered {code}"
        '';
      };
    });
  };
}
