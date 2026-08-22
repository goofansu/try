# Home Manager module for try.
#
#   programs.try.enable = true;
#
# A process cannot change its parent shell's directory, so the binary reports
# the destination and the shell function declared here performs the cd.
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.programs.try;
  try = lib.getExe cfg.package;
in
{
  options.programs.try = {
    enable = lib.mkEnableOption "try, a project directory jumper";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.try;
      defaultText = lib.literalExpression "pkgs.try";
      description = "The try package to use.";
    };

    enableFishIntegration = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Define the fish `try` function that changes directory.";
    };

    enableBashIntegration = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Define the bash `try` function that changes directory.";
    };

    enableZshIntegration = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Define the zsh `try` function that changes directory.";
    };
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
