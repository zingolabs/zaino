#!/bin/bash
for i in {4..12}
do
    echo "-----------" >> benchmark_for_tests.txt
    echo "STARTING runs for THREAD COUNT of*************************:" >> benchmark_for_tests.txt
    echo $i >> benchmark_for_tests.txt
    echo "*****************-----------************" >> benchmark_for_tests.txt
    for x in {1..10}
        do
            echo "-----------" >> benchmark_for_tests.txt
            echo "thread count:" >>benchmark_for_tests.txt
            echo $i >> benchmark_for_tests.txt
            echo "try:" >> benchmark_for_tests.txt
            echo $x >> benchmark_for_tests.txt
                cargo nextest run --test-threads=$i --cargo-quiet --cargo-quiet --retries 0 --failure-output final --final-status-level slow --status-level none --hide-progress-bar &>> benchmark_for_tests.txt
            echo "-----------" >> benchmark_for_tests.txt
        done
    echo "-----------" >> benchmark_for_tests.txt
    echo "ending runs for thread count of:" >> benchmark_for_tests.txt
    echo $i >> benchmark_for_tests.txt
    echo "-----------" >> benchmark_for_tests.txt
done
