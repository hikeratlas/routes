import type { NodeMap } from './types';
import * as lib from './lib';

const features = lib.load('../basemap/ztrails.geojsonl');
const map: NodeMap = lib.buildIndex(features);

for (const [ll, v] of Object.entries(map)) {
	const parking = v.nodes.filter(lib.isParkingLot);
	const paths = v.nodes.filter(lib.isFootPath);
	if (parking.length && paths.length) {
		console.log(ll);
		console.log(parking);
		console.log(paths);
	}
}
