docker create --name my_zaino_container zingodevops/zaino-ci:latest
docker cp my_zaino_container:/usr/local/bin ./testing/
mv ./testing/bin ./testing/binaries
docker rm my_zaino_container
