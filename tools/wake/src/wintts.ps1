# Windows' own voices (a third engine, never trained on): the phrase and
# near misses, at several speeds and pitches, with the Norwegian voice
# (Microsoft Jon, nb-NO) reading Norwegian spellings. Writes
# <Out>/{pos,neg}_*.wav.
param([string]$Out = "$PSScriptRoot\..\wintts")
New-Item -ItemType Directory -Force $Out | Out-Null
Add-Type -AssemblyName System.Speech
$s = New-Object System.Speech.Synthesis.SpeechSynthesizer
$fmt = New-Object System.Speech.AudioFormat.SpeechAudioFormatInfo(16000, [System.Speech.AudioFormat.AudioBitsPerSample]::Sixteen, [System.Speech.AudioFormat.AudioChannel]::Mono)
$en = @{
  pos = @("hashtag Hesteclip that", "hashtag Hesta clip that", "Hashtag Hesteclip that!", "hashtag, Hesteclip that.", "hashtag hestaclip that", "hashtag Hesteclip that?")
  neg = @("hashtag", "hashtag clip that", "Hesteclip that", "hashtag Hesteclip", "hashtag best clip that", "has the clip that", "clip that",
          "hashtag gaming", "Chester clip that", "hey Steve clip that", "hashtag blessed", "HasteClip that", "nice clip", "hashtag Hesteclip this", "the best clip")
}
$no = @{
  pos = @("hæsjtæg hesteklipp dæt", "hæsjtæg hesteklipp dett", "hæsjtæg, hesteklipp dæt!", "hæsjtæghesteklipp dæt", "hæsjtæg hesteklipp dæ")
  neg = @("hesteklipp", "klipp det", "hæsjtæg", "bare hest klipp da", "hæsjtæg klipp det", "hesteklipp dæt", "beste klipp", "hæsjtæg hest", "hæsjtæg hesteklipp")
}
$voices = @(@("Microsoft Zira Desktop", "en-US", $en), @("Microsoft David Desktop", "en-US", $en), @("Microsoft Mark", "en-US", $en), @("Microsoft Jon", "nb-NO", $no))
$rates = @(-2, 0, 2, 4, 6)
$pitches = @("x-low", "low", "medium", "high")
$i = 0
foreach ($v in $voices) {
  $s.SelectVoice($v[0])
  $lang = $v[1]
  $texts = $v[2]
  foreach ($r in $rates) {
    foreach ($p in $pitches) {
      foreach ($kind in @("pos", "neg")) {
        $list = $texts[$kind]
        $t = $list[$i % $list.Count]
        $ssml = "<speak version='1.0' xmlns='http://www.w3.org/2001/10/synthesis' xml:lang='$lang'><prosody pitch='$p'>$t</prosody></speak>"
        $s.Rate = $r
        $f = Join-Path $Out ("{0}_{1:d3}.wav" -f $kind, $i)
        $s.SetOutputToWaveFile($f, $fmt)
        $s.SpeakSsml($ssml)
        $s.SetOutputToNull()
      }
      $i++
    }
  }
}
"$i pairs"
