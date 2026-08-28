from .apple import AppleContainerRuntime
from .base import ManagedContainer, Runtime, RuntimeUnavailable
from .docker import DockerRuntime

_RUNTIMES = {
    "docker": DockerRuntime,
    "apple-container": AppleContainerRuntime,
}


def get_runtime(name: str) -> Runtime:
    return _RUNTIMES[name]()


__all__ = [
    "AppleContainerRuntime",
    "DockerRuntime",
    "ManagedContainer",
    "Runtime",
    "RuntimeUnavailable",
    "get_runtime",
]
