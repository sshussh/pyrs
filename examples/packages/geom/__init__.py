# Package init: re-export helpers from the shapes submodule.
from .shapes import circle_area, PI, VERSION

def package_version() -> int:
    return VERSION
