podman create --name my_zaino_container zingodevops/zaino-ci:latest
podman cp my_zaino_container:/usr/local/bin ./test_binaries/
mv ./test_binaries/bin ./test_binaries/bins
podman rm my_zaino_container
