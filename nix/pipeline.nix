pkgs: let
  inherit (pkgs) lib;

  plain = name: builtins.match "[A-Za-z0-9_-]{1,64}" name != null;

  entry = step:
    lib.filterAttrs (_: value: value != null) {
      inherit (step) name;
      agent = step.agent or null;
      optional = step.optional or null;
      timeout = step.timeout or null;
      secrets = step.secrets or null;
      tags = step.tags or null;
    };

  program = step:
    step.program or (pkgs.writeShellApplication {
      inherit (step) name;
      runtimeInputs = step.runtimeInputs or [];
      text = step.script;
    });

  android = import pkgs.path {
    inherit (pkgs.stdenv.hostPlatform) system;
    config.allowUnfree = true;
  };
  buildTools =
    (android.androidenv.composeAndroidPackages {
      buildToolsVersions = ["35.0.0"];
      platformVersions = [];
      includeEmulator = false;
    }).build-tools;
  zipalign = pkgs.runCommand "zipalign" {} ''
    mkdir -p $out/bin
    ln -s ${lib.head buildTools}/libexec/android-sdk/build-tools/35.0.0/zipalign $out/bin/zipalign
  '';

  notesInstructions = ''
    Estás escribiendo las notas de un build de prueba interno, a partir de commits de git en crudo. Quien las lee va a probar el build y necesita decidir qué revisar, no es una audiencia de marketing: sé conciso y honesto, y di claramente cuando el mensaje de un commit sugiera que algo puede estar incompleto o sin probar.

    Escribe TODO en español neutro de Latinoamérica, natural y correcto, con todas las tildes y la eñe. Deja en inglés únicamente los identificadores de código, los nombres de archivos y de librerías, y los ids de commit.

    Escribe exactamente tres cosas, nada más:

    1. Una lista de "Cambios": una viñeta markdown por cambio lógico, agrupando los commits que pertenecen al mismo cambio. Lenguaje llano, sin volcar rutas de archivos. Menciona el id del issue (como CHI-123) en la misma línea si el commit lo referencia.
    2. Una nota de "Errores conocidos": todo lo que los propios mensajes de commit marquen como roto, como workaround o como limitación conocida. Si no hay nada marcado, escribe exactamente: "Ninguno declarado en este rango de commits. Este resumen sale de los mensajes de commit, no de una pasada manual de QA." seguida de una oración que diga qué debería probar quien testee, si el diffstat apunta a algo riesgoso (un módulo nativo nuevo, un flujo de UI nuevo, autenticación o pagos).
    3. Un resumen "Técnico" para otro agente que tenga que entender o revisar este build sin leer el diff: una viñeta por cambio lógico con qué se hizo y cómo (módulos, servicios, componentes y archivos tocados, contratos o tipos que cambian, dependencias, flags y configuración, tests), seguida de los riesgos de regresión concretos. Aquí sí nombra rutas e identificadores; sé preciso y denso, no repitas "Cambios".

    Devuelve SOLO las viñetas de "Cambios", después una línea que contenga exactamente "===KNOWN_BUGS===", después el texto de "Errores conocidos", después una línea que contenga exactamente "===TECHNICAL===", y después el resumen "Técnico". Sin títulos, sin comentarios extra, sin bloques de código.
  '';
in rec {
  mkPipeline = {
    steps,
    name ? "chi-pipeline",
  }: let
    all = lib.flatten steps;
    runs = lib.filter (step: !(step ? agent)) all;
    names = map (step: step.name) all;
  in
    assert lib.assertMsg (lib.all plain names) "chi pipeline step names are letters, digits, _ and -, up to 64";
    assert lib.assertMsg (lib.allUnique names) "chi pipeline step names must be unique";
    assert lib.assertMsg (lib.all (step: step ? script || step ? program) runs) "a chi pipeline step needs script, program or agent";
      pkgs.runCommand name {passthru.manifest = {steps = map entry all;};} ''
        mkdir -p $out/bin
        cp ${pkgs.writeText "pipeline.json" (builtins.toJSON {steps = map entry all;})} $out/pipeline.json
        ${lib.concatMapStrings (step: "ln -s ${lib.getExe' (program step) step.name} $out/bin/${step.name}\n") runs}
      '';

  steps = {
    notesPrompt = {
      name ? "notes-prompt",
      instructions ? notesInstructions,
      maxDiffBytes ? 300000,
    }: {
      inherit name;
      runtimeInputs = [pkgs.jq pkgs.coreutils];
      script = ''
        {
          printf '%s\n' ${lib.escapeShellArg instructions}
          echo "Build: $(jq -r '"\(.repo) \(.version) (\(.branch)) \(.commit)"' "$CHI_IN/build.json")"
          echo
          echo "Commits de este build, del más antiguo al más reciente:"
          jq -r 'reverse | .[] | "commit \(.sha[0:7]): \(.subject)\n\(.body)\n"' "$CHI_IN/commits.json"
          echo "Diffstat:"
          cat "$CHI_IN/diffstat.txt"
          echo
          echo "Diff:"
          head -c ${toString maxDiffBytes} "$CHI_IN/diff.patch"
        } > "$CHI_WORK/notes-prompt.txt"
      '';
    };

    notesAgent = {
      name ? "notes-agent",
      cli ? "claude",
      model ? "",
      timeout ? 900,
    }: {
      inherit name timeout;
      agent = {
        inherit cli model;
        prompt = "notes-prompt.txt";
        output = "notes-raw.txt";
      };
    };

    notesFormat = {
      name ? "notes-format",
      changes ? "Cambios",
      knownBugs ? "Errores conocidos",
      technical ? "Técnico",
    }: {
      inherit name;
      runtimeInputs = [pkgs.gawk];
      script = ''
        awk -v changes=${lib.escapeShellArg changes} -v bugs=${lib.escapeShellArg knownBugs} -v technical=${lib.escapeShellArg technical} '
          BEGIN { print "## " changes }
          /^===KNOWN_BUGS===[[:space:]]*$/ { print "\n## " bugs; next }
          /^===TECHNICAL===[[:space:]]*$/ { print "\n## " technical; next }
          { print }
        ' "$CHI_WORK/notes-raw.txt" > "$CHI_WORK/notes.md"
      '';
    };

    releaseNotes = {
      cli ? "claude",
      model ? "",
      instructions ? notesInstructions,
    }: [
      (steps.notesPrompt {inherit instructions;})
      (steps.notesAgent {inherit cli model;})
      (steps.notesFormat {})
    ];

    driveUpload = {
      name ? "drive-upload",
      folderSecret ? "gdrive-folder",
      accountSecret ? "gdrive-account",
      tags ? null,
    }: {
      inherit name tags;
      secrets = [folderSecret accountSecret];
      timeout = 1800;
      runtimeInputs = [pkgs.gdrive3 pkgs.jq pkgs.coreutils];
      script = ''
        gdrive account import "$CHI_SECRETS/${accountSecret}" > /dev/null
        version=$(jq -r '"\(.version)-\(.id)"' "$CHI_IN/build.json")
        folder=$(gdrive files mkdir --parent "$(tr -d '[:space:]' < "$CHI_SECRETS/${folderSecret}")" --print-only-id "$version")
        shopt -s nullglob
        for file in "$CHI_IN"/artifacts/* "$CHI_WORK"/artifacts/*; do
          id=$(gdrive files upload --parent "$folder" --print-only-id "$file")
          echo "Drive: $(basename "$file") https://drive.google.com/file/d/$id/view" >> "$CHI_WORK/links"
        done
        echo "Drive https://drive.google.com/drive/folders/$folder" >> "$CHI_WORK/links"
      '';
    };

    apkVariants = {
      variants,
      name ? "apk-variants",
      keystoreSecret ? "keystore",
      passwordSecret ? "keystore-password",
      tags ? null,
    }:
      assert lib.assertMsg (lib.all (v: plain v.suffix && builtins.match "[A-Za-z][A-Za-z0-9_]*(\\.[A-Za-z][A-Za-z0-9_]*)+" v.package != null) variants)
      "apk variants need a plain suffix and a valid application id"; {
        inherit name tags;
        secrets = [keystoreSecret passwordSecret];
        timeout = 1800;
        runtimeInputs = [pkgs.apktool pkgs.apksigner zipalign pkgs.perl pkgs.coreutils];
        script = ''
          shopt -s nullglob
          for apk in "$CHI_IN"/artifacts/*.apk; do
            base=$(basename "$apk" .apk)
            rm -rf /tmp/decoded
            apktool d -f -s -o /tmp/decoded "$apk" > /dev/null
            from=$(perl -ne 'print $1 and exit if /<manifest[^>]*\bpackage="([^"]+)"/' /tmp/decoded/AndroidManifest.xml)
            while IFS=$'\t' read -r suffix to label; do
              rm -rf /tmp/variant && cp -r /tmp/decoded /tmp/variant
              FROM="$from" TO="$to" perl -pi -e '
                my ($f, $t) = ($ENV{FROM}, $ENV{TO});
                s/\bpackage="\Q$f\E"/package="$t"/;
                s{android:authorities="([^"]*)"}{"android:authorities=\"" . join(";", map { s/^\Q$f\E(?=\.|$)/$t/r } split /;/, $1) . "\""}ge;
                s/(<(?:uses-)?permission\s+android:name=")\Q$f\E\./$1$t./g;
              ' /tmp/variant/AndroidManifest.xml
              if [ -n "$label" ] && [ -f /tmp/variant/res/values/strings.xml ]; then
                LABEL="$label" perl -pi -e 'BEGIN { ($l = $ENV{LABEL}) =~ s/&/&amp;/g; $l =~ s/</&lt;/g; $l =~ s/([\x27"])/\\$1/g } s{(<string name="app_name"[^>]*>)[^<]*}{$1$l}' /tmp/variant/res/values/strings.xml
              fi
              apktool b -o /tmp/unsigned.apk /tmp/variant > /dev/null
              zipalign -f -p 4 /tmp/unsigned.apk /tmp/aligned.apk
              mkdir -p "$CHI_WORK/artifacts"
              out="$CHI_WORK/artifacts/$base-$suffix.apk"
              apksigner sign --ks "$CHI_SECRETS/${keystoreSecret}" --ks-pass "file:$CHI_SECRETS/${passwordSecret}" --out "$out" /tmp/aligned.apk
              apksigner verify "$out"
              echo "$base-$suffix.apk: $from -> $to"
            done <<'VARIANTS'
          ${lib.concatMapStrings (v: "${v.suffix}\t${v.package}\t${v.label or ""}\n") variants}VARIANTS
          done
        '';
      };
  };
}
