/no_think
Describe ONLY what you actually see in this image. Do not guess intent or backstory. Be literal. If the subject looks hunched, sad, or distorted, say so. When a person is the main subject, name them naturally as 'the woman', 'the man', or 'the person', never 'a figure'.

Answer with ONE JSON object and nothing else. Use exactly these keys:

"shortCaption": one sentence saying what the image shows.

"longCaption": a detailed, precise, literal description of everything visible, in several paragraphs: framing and camera angle; lighting; each person (body, face, hair, expression, posture, attire, what they are doing and where they look); every object and surface; the setting; colour; and especially the MEDIUM (photo / oil / charcoal / 3D render) and the visual STYLE. Say how things relate to each other spatially and physically.

"sceneObjects": a JSON list of every distinct, physically segmentable thing visible, as concrete nouns a segmentation model could outline: person, hand, eye, hair, harp, rock, doorway, sky. List each kind once. Exclude abstract or style words such as lighting, mood, medium, composition, style.

"sceneCamera": camera angle, a short phrase (low angle, eye level, overhead).
"sceneFraming": shot framing (close-up, full body, wide).
"scenePosture": body posture (standing, hunched, seated).
"sceneGaze": where the subject looks, as ONE coded value from this list and nothing else: gaze-looking-at-viewer, gaze-looking-away, gaze-looking-up, gaze-looking-down, gaze-looking-left, gaze-looking-right, gaze-looking-at-other, gaze-looking-at-object, gaze-looking-over-shoulder, gaze-eyes-closed, gaze-obscured.
"sceneExpression": facial expression (calm, alert, sad).
"sceneAction": what is happening (reading, reaching, still).
"sceneEnergy": energy level (quiet, tense, dynamic).
"sceneMood": overall mood (serene, ominous, playful).
"sceneLighting": lighting (soft dusk, harsh, backlit).
"sceneStyle": visual style (painterly, photoreal, surreal).
"sceneMedium": medium (photo, oil, charcoal, 3D render).
"sceneLocation": where (studio, cliffside, server room).
"sceneSubjectOrientation": which way the main subject's body faces the camera, ONE of: front, back, side, side-front, side-back.

"imageTechnicalScore": how well MADE this image is, a number from 0 to 100. Judge craft only: focus and sharpness, exposure, noise, banding, compression artefacts, blown highlights, crushed shadows. A technically flawless image of nothing scores high here.
"imageTechnicalCritique": one sentence saying why that score, naming what you actually saw.
"imageAestheticScore": how good this image is to LOOK AT, a number from 0 to 100. Judge composition, framing, light, colour, gesture, whether it holds attention. A blurry snapshot of a remarkable moment can score high here.
"imageAestheticCritique": one sentence saying why that score, naming what you actually saw.
"imageAnatomyScore": how anatomically correct the bodies in this image are, a number from 0 to 100, where 100 means nothing is wrong. Count fingers, hands, arms, legs, feet, eyes and heads. Look for limbs that join nowhere, joints bending the wrong way, a duplicated or merged body part, a hand with the wrong number of fingers. Judge animals the same way. Lower the score the worse and the more obvious the error is. Omit this key and the next one if the image contains no body at all.
"imageAnatomyCritique": one sentence naming the anatomical error you saw, or saying you saw none.

Omit a scene key only if it truly does not apply. Do not add keys.
