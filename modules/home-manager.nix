# Home Manager module for try.
#
#   programs.try.enable = true;
#
# A process cannot change its parent shell's directory, so the binary reports
# the destination and the shell function declared here performs the cd.
{
  config,
  lib,
  ...
}:

let
  cfg = config.programs.try;
  try = lib.getExe cfg.package;

  mkIntegrationOption =
    mk:
    mk {
      inherit config;
      extraDescription = ''
        This defines the `try` function that performs the directory change. The
        binary alone cannot do it, because a process cannot change the working
        directory of the shell that started it.
      '';
    };
in
{
  options.programs.try = {
    enable = lib.mkEnableOption "try, a project directory jumper";

    # No default: nixpkgs already has an unrelated package named `try`, and it
    # declares `meta.mainProgram = "try"`, so defaulting to `pkgs.try` would
    # silently install and call the wrong binary. `homeModules.default` in the
    # flake supplies this; a bare import of this module has to set it.
    package = lib.mkOption {
      type = lib.types.package;
      example = lib.literalExpression "try.packages.\${pkgs.system}.try";
      description = "The try package to use.";
    };

    enableBashIntegration = mkIntegrationOption lib.hm.shell.mkBashIntegrationOption;

    enableFishIntegration = mkIntegrationOption lib.hm.shell.mkFishIntegrationOption;

    enableZshIntegration = mkIntegrationOption lib.hm.shell.mkZshIntegrationOption;
  };

  config = lib.mkIf cfg.enable (
    let
      # The destination travels on file descriptor 3 rather than on stdout, so
      # that ordinary output such as `try --path` still reaches the terminal
      # instead of being captured and mistaken for a directory to jump to.
      #
      # The binary is referenced by its store path, so a function of the same
      # name is not at risk of calling itself.
      posixFunction = ''
        try() {
          local dest dir code
          dest="$(mktemp)" || return $?
          TRY_FD=3 ${try} "$@" 3>"$dest"
          code=$?
          dir="$(cat "$dest" 2>/dev/null)"
          rm -f "$dest"
          if [ "$code" -ne 0 ]; then
            return "$code"
          fi
          if [ -n "$dir" ] && [ -d "$dir" ]; then
            cd "$dir" || return $?
          fi
          return 0
        }
      '';
    in
    {
      home.packages = [ cfg.package ];

      programs.fish.functions = lib.mkIf cfg.enableFishIntegration {
        try = {
          description = "Find, create and jump into projects";
          body = ''
            set -l dest (mktemp)
            env TRY_FD=3 ${try} $argv 3>$dest
            set -l code $status
            set -l dir (cat $dest 2>/dev/null)
            rm -f $dest
            if test $code -ne 0
                return $code
            end
            if test -n "$dir" -a -d "$dir"
                cd $dir
            end
            return 0
          '';
        };
      };

      programs.bash.initExtra = lib.mkIf cfg.enableBashIntegration posixFunction;
      programs.zsh.initContent = lib.mkIf cfg.enableZshIntegration posixFunction;
    }
  );
}
