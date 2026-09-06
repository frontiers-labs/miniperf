cmake_minimum_required(VERSION 3.14)
project(miniperf-dr-decode-coverage C)

find_package(DynamoRIO REQUIRED)

add_executable(dr_decode_probe probe.c)
configure_DynamoRIO_decoder(dr_decode_probe)
