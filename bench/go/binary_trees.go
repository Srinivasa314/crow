package main

import "fmt"

type Tree struct {
	left  *Tree
	right *Tree
	value int
}

func build(depth int, value int) *Tree {
	if depth == 0 {
		return &Tree{value: value}
	}
	return &Tree{
		left:  build(depth-1, value*2),
		right: build(depth-1, value*2+1),
		value: value,
	}
}

func sum(t *Tree) int {
	if t == nil {
		return 0
	}
	return t.value + sum(t.left) + sum(t.right)
}

func main() {
	total := 0
	for i := 0; i < 160; i++ {
		total += sum(build(14, i+1))
	}
	fmt.Println(total)
}

