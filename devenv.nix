{
  pkgs,
  ...
}:

{
  # https://devenv.sh/packages/
  packages = with pkgs; [
    python3
    uv
    python314Packages.python-osc
  ];
}
