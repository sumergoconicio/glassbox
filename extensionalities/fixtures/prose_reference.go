// Standalone reference prose scorer matching extensionalities/crates/litx-inspect/src/profile.rs.
// Run directly with `go run extensionalities/fixtures/prose_reference.go`. Writes JSON to stdout.
package main

import (
	"encoding/json"
	"os"
	"strings"
	"unicode"
)

const stopwordList = "a about above after again against all am an and any are as at be because been before being below between both but by can could did do does doing down during each few for from further had has have having he her here hers herself him himself his how i if in into is it its itself just me more most my myself no nor not now of off on once only or other our ours ourselves out over own same should so some such than that the their theirs them themselves then there these they this those through to too under until up very was we were what when where which while who whom why will with would you your yours yourself yourselves"

var stopwords = func() map[string]struct{} {
	fields := strings.Split(stopwordList, " ")
	set := make(map[string]struct{}, len(fields))
	for _, word := range fields {
		set[word] = struct{}{}
	}
	return set
}()

func toASCIILower(s string) string {
	var b strings.Builder
	b.Grow(len(s))
	for i := 0; i < len(s); i++ {
		c := s[i]
		if c >= 'A' && c <= 'Z' {
			c += 'a' - 'A'
		}
		b.WriteByte(c)
	}
	return b.String()
}

// calculateProseDensity computes the bounded prose density score in [0, 1]
// matching extensionalities/crates/litx-inspect/src/profile.rs::prose_density.
func calculateProseDensity(value string) float64 {
	text := strings.TrimSpace(value)
	if len(text) < 15 {
		return 0.0
	}
	words := 0
	stops := 0
	symbols := 0
	sentences := 0
	sentenceWords := 0
	valid := true

	for _, word := range strings.Fields(text) {
		words++
		for i := 0; i < len(word); i++ {
			if strings.IndexByte("{}[]();=<>\\|~^`", word[i]) >= 0 {
				symbols++
			}
		}
		cleaned := strings.Trim(word, ".,!?:;\"'()[]{}-_")
		if _, isStop := stopwords[toASCIILower(cleaned)]; isStop {
			stops++
		} else if stops == 0 && valid {
			letters := strings.TrimRight(word, ".!?")
			if letters == "" {
				valid = false
			} else {
				first := true
				for _, r := range letters {
					if !unicode.IsLetter(r) {
						valid = false
						break
					}
					if first && sentenceWords == 0 && !unicode.IsUpper(r) {
						valid = false
						break
					}
					first = false
				}
			}
			if valid {
				sentenceWords++
				if len(letters) != len(word) {
					if sentenceWords < 3 {
						valid = false
					} else {
						sentences++
						sentenceWords = 0
					}
				}
			}
		}
	}

	if words < 3 {
		return 0.0
	}

	ratio := 0.0
	if stops > 0 {
		ratio = float64(stops) / float64(words)
	} else if valid && sentenceWords == 0 && sentences > 0 {
		ratio = float64(sentences) / float64(words)
	}

	symbolPenalty := (float64(symbols) / float64(len(text))) * 4.0
	if symbolPenalty > 1.0 {
		symbolPenalty = 1.0
	}
	lengthScale := float64(len(text)) / 120.0
	if lengthScale > 1.0 {
		lengthScale = 1.0
	}
	score := (ratio * 2.2) * (1.0 - symbolPenalty) * lengthScale
	if score < 0.0 {
		return 0.0
	}
	if score > 1.0 {
		return 1.0
	}
	return score
}

func main() {
	type sample struct {
		Text  string  `json:"text"`
		Score float64 `json:"score"`
	}
	texts := []string{
		"", "short", "alpha beta gamma", "Alpha beta gamma.",
		"We can inspect the original source and verify the evidence in this answer.",
		"This is another native note about the source and its evidence.",
		`if (x == y) { return a; }`, "the and of", "  This is a sentence.  ",
		"Étude précise demeure.", "École régionale ferme.",
		"Ⅳ alpha beta.", "中文 文本 测试。", "Alpha\u0085beta\u2003gamma.",
		"“the” unquoted words.",
	}
	result := struct {
		UnicodeVersion string   `json:"unicode_version"`
		Samples        []sample `json:"samples"`
	}{UnicodeVersion: unicode.Version}
	for _, text := range texts {
		result.Samples = append(result.Samples, sample{text, calculateProseDensity(text)})
	}
	encoder := json.NewEncoder(os.Stdout)
	encoder.SetIndent("", "  ")
	if err := encoder.Encode(result); err != nil {
		panic(err)
	}
}
