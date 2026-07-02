package main

import "fmt"

func main() {
	checksum := 0
	width := 900
	height := 600
	limit := 100

	for py := 0; py < height; py++ {
		y0 := float64(py)/float64(height)*2.0 - 1.0
		for px := 0; px < width; px++ {
			x0 := float64(px)/float64(width)*3.5 - 2.5
			x := 0.0
			y := 0.0
			iter := 0
			for x*x+y*y <= 4.0 && iter < limit {
				xt := x*x - y*y + x0
				y = 2.0*x*y + y0
				x = xt
				iter++
			}
			checksum += iter
		}
	}

	fmt.Println(checksum)
}

