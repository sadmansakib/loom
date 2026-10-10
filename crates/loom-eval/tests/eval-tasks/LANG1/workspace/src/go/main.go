package main

import (
	"fmt"
	"os"
)

func load(path string) string {
	data, _ := os.ReadFile(path)
	return string(data)
}

func main() {
	fmt.Println(load("input.txt"))
}
