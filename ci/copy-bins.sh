docker create --name my_zaino_container zingodevops/zaino-ci:latest
docker cp my_zaino_container:/usr/local/bin ./test_binaries/
mv ./test_binaries/bin ./test_binaries/bins
docker rm my_zaino_container
